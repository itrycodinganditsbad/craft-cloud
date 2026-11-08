//! The Shared tab and the admin pages' Team spaces part as kittest draws them (snapshots `shared-*` and `admin-spaces-*`
//! in `tests/snapshots/`, written with `UPDATE_SNAPSHOTS=1`), and that their buttons send what they should and the
//! owner-only controls are not drawn for anyone else. The state and the rules are tested where they live
//! ([`crate::spaces`], [`crate::admin`]).

use craft_cloud_app::{SpaceId, SpaceRole, SpaceRun, SpaceSync, SpaceUsage, Uuid};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use super::*;
use crate::admin::tests::{agent as admin_agent, run, signed_in};
use crate::admin::{AdminEvent, Section, View};
use crate::agent::fake::{FakeAgent, admin_space, team_member, team_space};
use crate::model::{Model, Screen, Tab, tests::status};
use crate::spaces::{Deletes, SpacesMsg};

/// The time the screens are drawn at.
const NOW: i64 = 1_790_000_000_000;
/// A window tall enough for a whole page at the smallest width (kittest clicks and draws only what is on screen).
const SMALL: [f32; 2] = [crate::ui::MIN_WINDOW_SIZE[0], 1000.0];

fn harness_sized(model: Model, theme: egui::Theme, size: [f32; 2]) -> Harness<'static, (Model, Vec<Event>)> {
    Harness::builder().with_size(egui::Vec2::from(size)).with_theme(theme).wgpu_setup(super::tests::shared_wgpu()).build_ui_state(
        |ui, (model, events): &mut (Model, Vec<Event>)| {
            events.extend(show(ui, model, NOW));
        },
        (model, Vec::new()),
    )
}

/// A window tall enough for a whole page to be on screen (kittest clicks only what is).
fn harness(model: Model) -> Harness<'static, (Model, Vec<Event>)> {
    harness_sized(model, egui::Theme::Light, [WINDOW_SIZE[0], 1800.0])
}

fn shot(model: &Model, theme: egui::Theme, name: &str, size: [f32; 2]) {
    let mut h = harness_sized(model.clone(), theme, size);
    h.run_steps(4);
    let image = h.render().unwrap();
    super::tests::compare(image, name);
}

/// The window at its usual size, in the light and the dark theme, and at the smallest width.
fn shots(model: &Model, name: &str) {
    shot(model, egui::Theme::Light, name, [WINDOW_SIZE[0], 900.0]);
    shot(model, egui::Theme::Dark, &format!("{name}-dark"), [WINDOW_SIZE[0], 900.0]);
    shot(model, egui::Theme::Light, &format!("{name}-small"), SMALL);
}

fn sample_usage(stored: u64, quota: Option<u64>) -> SpaceUsage {
    SpaceUsage {
        stored_bytes: stored,
        quota_bytes: quota,
        trash_bytes: None,
        history_bytes: None,
        live_bytes: 0,
        estimates: craft_cloud_app::UsageEstimates { index_complete: true, reason: None },
    }
}

/// The spaces of a person who is in four team spaces and has two invitations.
struct World {
    agent: FakeAgent,
    studio: SpaceId,
    brand: SpaceId,
    archive: SpaceId,
    lost: SpaceId,
    campaign: SpaceId,
}

fn world() -> World {
    let mut studio = team_space("Studio", SpaceRole::Editor, "active");
    studio.sync = SpaceSync::On;
    studio.run_state = Some(SpaceRun::Syncing);
    studio.last_sync_ms = Some(NOW - 90_000);
    let brand = team_space("Brand identity", SpaceRole::Owner, "active");
    let mut archive = team_space("Archive 2025", SpaceRole::Viewer, "active");
    archive.archived = true;
    archive.read_only = true;
    let mut lost = team_space("Print shop", SpaceRole::Editor, "active");
    lost.sync = SpaceSync::Unavailable;
    lost.run_state =
        Some(SpaceRun::NoAccess { message: "You no longer have access to Print shop. Your local copy is kept and no longer syncs.".into() });
    let campaign = team_space("Spring campaign", SpaceRole::Viewer, "invited");
    let packaging = team_space("Packaging", SpaceRole::Editor, "invited");
    let ids = (studio.space, brand.space, archive.space, lost.space, campaign.space);
    let agent = FakeAgent::new(status(true));
    {
        let mut team = agent.team.lock().unwrap();
        team.spaces = vec![studio, brand, archive, lost, campaign, packaging];
        team.usage = vec![
            (ids.0, sample_usage(3_400_000_000, Some(10 << 30))),
            (ids.1, sample_usage(120_000_000, None)),
            (ids.2, sample_usage(9_800_000_000, Some(10 << 30))),
        ];
        team.members = vec![
            (
                ids.1,
                vec![
                    team_member("fabio", SpaceRole::Owner, "active", 1),
                    team_member("ana", SpaceRole::Editor, "active", 2),
                    team_member("ben", SpaceRole::Viewer, "active", 3),
                    team_member("cleo.m", SpaceRole::Editor, "invited", 4),
                ],
            ),
            (ids.0, vec![team_member("ana", SpaceRole::Owner, "active", 5), team_member("fabio", SpaceRole::Editor, "active", 6)]),
            (ids.2, vec![team_member("ana", SpaceRole::Owner, "active", 5), team_member("fabio", SpaceRole::Viewer, "active", 6)]),
        ];
    }
    World { agent, studio: ids.0, brand: ids.1, archive: ids.2, lost: ids.3, campaign: ids.4 }
}

fn on_shared(agent: &FakeAgent) -> Model {
    let mut model = signed_in(agent);
    run(&mut model, agent, vec![Event::Tab(Tab::Shared)]);
    model
}

fn open(agent: &FakeAgent, id: SpaceId) -> Model {
    let mut model = on_shared(agent);
    run(&mut model, agent, vec![Event::Spaces(SpacesMsg::OpenSpace(id))]);
    model
}

fn events_of(h: &mut Harness<'static, (Model, Vec<Event>)>) -> Vec<Event> {
    h.run_steps(2);
    std::mem::take(&mut h.state_mut().1)
}

// ---- the screens ----

#[test]
fn snapshot_shared_list_with_invitations() {
    let w = world();
    shots(&on_shared(&w.agent), "shared-list");
}

#[test]
fn snapshot_shared_empty_loading_and_failed() {
    let none = FakeAgent::new(status(true));
    shot(&on_shared(&none), egui::Theme::Light, "shared-empty", [WINDOW_SIZE[0], 640.0]);
    shot(&on_shared(&none), egui::Theme::Dark, "shared-empty-dark", [WINDOW_SIZE[0], 640.0]);
    shot(&on_shared(&none), egui::Theme::Light, "shared-empty-small", crate::ui::MIN_WINDOW_SIZE);
    // Asked, not answered yet.
    let mut loading = signed_in(&none);
    loading.update(Event::Tab(Tab::Shared));
    shot(&loading, egui::Theme::Light, "shared-loading", [WINDOW_SIZE[0], 640.0]);
    // The agent can't be reached.
    let broken = FakeAgent::new(status(true));
    let mut model = signed_in(&broken);
    broken.team.lock().unwrap().fails.push(craft_cloud_app::Error::Io("the connection broke".into()));
    run(&mut model, &broken, vec![Event::Tab(Tab::Shared)]);
    shot(&model, egui::Theme::Light, "shared-failed", [WINDOW_SIZE[0], 640.0]);
}

#[test]
fn snapshot_shared_when_the_agent_is_too_old() {
    let old = FakeAgent::new(status(true));
    old.team.lock().unwrap().too_old = true;
    let model = on_shared(&old);
    shot(&model, egui::Theme::Light, "shared-too-old", [WINDOW_SIZE[0], 640.0]);
    shot(&model, egui::Theme::Dark, "shared-too-old-dark", [WINDOW_SIZE[0], 640.0]);
    shot(&model, egui::Theme::Light, "shared-too-old-small", crate::ui::MIN_WINDOW_SIZE);
}

#[test]
fn snapshot_shared_accepting_offers_to_sync_here() {
    let w = world();
    let mut model = on_shared(&w.agent);
    run(&mut model, &w.agent, vec![Event::Spaces(SpacesMsg::Accept(w.campaign))]);
    shots(&model, "shared-offer");
    // A folder that has files in it asks again.
    model.update(Event::FolderPicked(Some("/data/Spring campaign".into())));
    w.agent.team.lock().unwrap().folder_with_files = Some("/data/Spring campaign".into());
    run(&mut model, &w.agent, vec![Event::Spaces(SpacesMsg::OfferSync)]);
    shot(&model, egui::Theme::Light, "shared-offer-files", [WINDOW_SIZE[0], 900.0]);
}

#[test]
fn snapshot_a_space_page_of_an_owner_with_its_members() {
    let w = world();
    let model = open(&w.agent, w.brand);
    shots(&model, "shared-space-owner");
    let mut asking = model.clone();
    asking.update(Event::Spaces(SpacesMsg::AskRemove(Uuid::from_u128(0x7003))));
    shot(&asking, egui::Theme::Light, "shared-space-remove", [WINDOW_SIZE[0], 1100.0]);
    let mut leaving = model;
    leaving.update(Event::Spaces(SpacesMsg::AskLeave));
    shot(&leaving, egui::Theme::Light, "shared-space-leave", [WINDOW_SIZE[0], 1100.0]);
}

#[test]
fn snapshot_a_syncing_space_with_held_deletes() {
    let w = world();
    w.agent.team.lock().unwrap().held = vec!["Shots/010.psd".into(), "Shots/020.psd".into(), "Notes/brief.txt".into()];
    let mut model = open(&w.agent, w.studio);
    shot(&model, egui::Theme::Light, "shared-space-editor", [WINDOW_SIZE[0], 900.0]);
    run(&mut model, &w.agent, vec![Event::Spaces(SpacesMsg::CheckDeletes)]);
    assert!(matches!(model.shared_screen().and_then(|s| s.page()).map(|p| p.deletes.clone()), Some(Deletes::Asking(_))));
    shots(&model, "shared-space-deletes");
}

#[test]
fn snapshot_a_read_only_archived_and_a_lost_space() {
    let w = world();
    let archived = open(&w.agent, w.archive);
    shots(&archived, "shared-space-archived");
    shot(&open(&w.agent, w.lost), egui::Theme::Light, "shared-space-lost", [WINDOW_SIZE[0], 640.0]);
    let viewer = world();
    {
        let mut team = viewer.agent.team.lock().unwrap();
        if let Some(space) = team.spaces.iter_mut().find(|s| s.space == viewer.archive) {
            space.archived = false;
            space.read_only = true;
            space.sync = SpaceSync::On;
            space.run_state = Some(SpaceRun::Idle);
        }
    }
    shot(&open(&viewer.agent, viewer.archive), egui::Theme::Light, "shared-space-viewer", [WINDOW_SIZE[0], 900.0]);
}

#[test]
fn snapshot_the_tab_and_the_overview_show_the_waiting_invitations() {
    let w = world();
    let model = signed_in(&w.agent);
    assert_eq!(model.overview().unwrap().invitations, 2);
    shot(&model, egui::Theme::Light, "shared-invitations-on-the-overview", [WINDOW_SIZE[0], 640.0]);
    shot(&model, egui::Theme::Dark, "shared-invitations-on-the-overview-dark", [WINDOW_SIZE[0], 640.0]);
    shot(&model, egui::Theme::Light, "shared-invitations-on-the-overview-small", crate::ui::MIN_WINDOW_SIZE);
}

// ---- the buttons ----

#[test]
fn the_list_buttons_send_accept_decline_open_and_refresh() {
    let w = world();
    let mut h = harness(on_shared(&w.agent));
    h.run_steps(2);
    let accepts: Vec<_> = h.query_all_by_label("Accept").collect();
    assert_eq!(accepts.len(), 2, "one per invitation");
    accepts[0].click();
    let events = events_of(&mut h);
    assert!(events.contains(&Event::Spaces(SpacesMsg::Accept(w.campaign))), "{events:?}");
    h.query_all_by_label("Decline").nth(1).unwrap().click();
    let events = events_of(&mut h);
    assert!(events.iter().any(|e| matches!(e, Event::Spaces(SpacesMsg::Decline(id)) if *id != w.campaign)), "{events:?}");
    // Open: the four joined spaces, by name.
    let opens: Vec<_> = h.query_all_by_label("Open").collect();
    assert_eq!(opens.len(), 4);
    opens[0].click();
    let events = events_of(&mut h);
    assert!(events.iter().any(|e| matches!(e, Event::Spaces(SpacesMsg::OpenSpace(_)))), "{events:?}");
    h.get_by_label("Refresh").click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::Reload)));
}

#[test]
fn the_offer_buttons_send_sync_choose_and_not_now() {
    let w = world();
    let mut model = on_shared(&w.agent);
    run(&mut model, &w.agent, vec![Event::Spaces(SpacesMsg::Accept(w.campaign))]);
    let mut h = harness(model);
    h.run_steps(2);
    // The offer is above the table, whose "Sync" column header has the same label.
    h.query_all_by_label("Sync").next().unwrap().click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::OfferSync)));
    h.get_by_label("Choose another folder…").click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::OfferChoose)));
    h.get_by_label("Not now").click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::OfferSkip)));
}

#[test]
fn an_owners_page_has_the_owner_controls_and_the_buttons_send_their_events() {
    let w = world();
    let mut h = harness(open(&w.agent, w.brand));
    h.run_steps(2);
    for label in ["Rename", "Invite", "Leave this space", "Open folder"] {
        assert!(h.query_by_label(label).is_some(), "{label}");
    }
    assert_eq!(h.query_all_by_label("Remove").count(), 3, "one per member but the account itself");
    h.get_by_label("Invite").click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::SubmitInvite)));
    h.get_by_label("Rename").click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::StartRename)));
    h.get_by_label("Leave this space").click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::AskLeave)));
    h.get_by_label("Open folder").click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::OpenFolder(w.brand))));
    h.get_by_label("Sync on this computer").click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::SyncOn(w.brand))));
    h.query_all_by_label("Remove").next().unwrap().click();
    assert!(events_of(&mut h).iter().any(|e| matches!(e, Event::Spaces(SpacesMsg::AskRemove(_)))));
}

#[test]
fn a_viewers_page_draws_no_owner_controls() {
    let w = world();
    let mut h = harness(open(&w.agent, w.archive));
    h.run_steps(2);
    for label in ["Rename", "Invite", "Remove", "Invite by account name"] {
        assert!(h.query_by_label(label).is_none(), "{label} is for owners");
    }
    assert!(h.query_by_label("Leave this space").is_some(), "any role may leave");
    // An editor's page has none either.
    let mut h = harness(open(&w.agent, w.studio));
    h.run_steps(2);
    for label in ["Rename", "Invite", "Remove"] {
        assert!(h.query_by_label(label).is_none(), "{label} is for owners");
    }
    assert!(h.query_by_label("Stop syncing here").is_some());
    assert!(h.query_by_label("Approve deletes").is_some(), "an editor's synced space can hold deletes");
}

#[test]
fn the_held_deletes_question_sends_confirm_and_cancel() {
    let w = world();
    w.agent.team.lock().unwrap().held = vec!["a.txt".into(), "b.txt".into()];
    let mut model = open(&w.agent, w.studio);
    run(&mut model, &w.agent, vec![Event::Spaces(SpacesMsg::CheckDeletes)]);
    let mut h = harness(model);
    h.run_steps(2);
    assert!(h.query_by_label_contains("2 deletions are held back").is_some());
    h.get_by_label("Keep them held").click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::CancelDeletes)));
    h.get_by_label("Approve deletes").click();
    assert!(events_of(&mut h).contains(&Event::Spaces(SpacesMsg::ConfirmDeletes)));
}

#[test]
fn the_header_says_how_many_invitations_wait_and_the_overview_links_to_them() {
    let w = world();
    let mut h = harness(signed_in(&w.agent));
    h.run_steps(2);
    assert!(h.query_by_label("Shared · 2").is_some(), "the Shared tab carries the count");
    assert!(h.query_by_label_contains("2 invitations to team spaces").is_some());
    h.get_by_label("Shared ↗").click();
    assert!(events_of(&mut h).contains(&Event::Tab(Tab::Shared)));
    // No invitations: a plain tab and no tag.
    let none = FakeAgent::new(status(true));
    let mut h = harness(signed_in(&none));
    h.run_steps(2);
    assert!(h.query_by_label("Shared").is_some() && h.query_by_label_contains("invitation").is_none());
}

#[test]
fn an_old_agent_shows_one_message_and_no_buttons() {
    let old = FakeAgent::new(status(true));
    old.team.lock().unwrap().too_old = true;
    let mut h = harness(on_shared(&old));
    h.run_steps(2);
    assert!(h.query_by_label_contains("Update Craft Cloud on this computer to use team spaces").is_some());
    for label in ["Accept", "Refresh", "Open"] {
        assert!(h.query_by_label(label).is_none(), "{label}");
    }
}

// ---- the admin pages' Team spaces part ----

fn spaces_agent() -> FakeAgent {
    let boss = admin_agent(true, 3);
    boss.team.lock().unwrap().admin_spaces = vec![
        admin_space("Brand identity", 4, 120_000_000, None),
        admin_space("Packaging", 2, 4_800_000_000, Some(5 << 30)),
        {
            let mut archived = admin_space("Archive 2025", 6, 9_900_000_000, Some(10 << 30));
            archived.archived_ms = Some(NOW - 86_400_000);
            archived
        },
        {
            let mut orphaned = admin_space("Print shop", 3, 0, Some(512 << 20));
            orphaned.owners = 0;
            orphaned
        },
    ];
    boss
}

fn admin_spaces(boss: &FakeAgent) -> Model {
    let mut model = signed_in(boss);
    run(&mut model, boss, vec![Event::Admin(AdminEvent::Open), Event::Admin(AdminEvent::Show(Section::Spaces))]);
    model
}

fn typed_form(model: &mut Model, name: &str, owner: &str, limit: &str) {
    if let Screen::Admin(admin) = &mut model.screen
        && let View::CreateSpace(form) = &mut admin.view
    {
        (form.name, form.owner, form.limit) = (name.to_owned(), owner.to_owned(), limit.to_owned());
    }
}

#[test]
fn snapshot_the_admin_team_spaces_list_and_the_new_space_form() {
    let boss = spaces_agent();
    let model = admin_spaces(&boss);
    shots(&model, "admin-spaces-list");
    let none = admin_agent(true, 1);
    shot(&admin_spaces(&none), egui::Theme::Light, "admin-spaces-empty", [WINDOW_SIZE[0], 640.0]);
    let mut form = model.clone();
    run(&mut form, &boss, vec![Event::Admin(AdminEvent::NewSpace)]);
    typed_form(&mut form, "Studio", "user01", "20480");
    shots(&form, "admin-spaces-new");
    run(&mut form, &boss, vec![Event::Admin(AdminEvent::SubmitSpace)]);
    shot(&form, egui::Theme::Light, "admin-spaces-confirm", [WINDOW_SIZE[0], 640.0]);
    shot(&form, egui::Theme::Dark, "admin-spaces-confirm-dark", [WINDOW_SIZE[0], 640.0]);
    let mut unknown = model;
    run(&mut unknown, &boss, vec![Event::Admin(AdminEvent::NewSpace)]);
    typed_form(&mut unknown, "Studio", "nobody", "");
    run(&mut unknown, &boss, vec![Event::Admin(AdminEvent::SubmitSpace)]);
    shot(&unknown, egui::Theme::Light, "admin-spaces-new-unknown-owner", [WINDOW_SIZE[0], 640.0]);
}

#[test]
fn snapshot_an_admin_team_space_page() {
    let boss = spaces_agent();
    let id = |name: &str, model: &Model| model.admin_screen().unwrap().spaces.spaces.iter().find(|s| s.name == name).unwrap().id;
    let model = admin_spaces(&boss);
    let (packaging, archive, print) = (id("Packaging", &model), id("Archive 2025", &model), id("Print shop", &model));
    let mut page = model.clone();
    run(&mut page, &boss, vec![Event::Admin(AdminEvent::OpenSpace(packaging))]);
    shots(&page, "admin-spaces-page");
    let mut archived = model.clone();
    run(&mut archived, &boss, vec![Event::Admin(AdminEvent::OpenSpace(archive))]);
    shot(&archived, egui::Theme::Light, "admin-spaces-page-archived", [WINDOW_SIZE[0], 1000.0]);
    let mut orphan = model;
    run(&mut orphan, &boss, vec![Event::Admin(AdminEvent::OpenSpace(print))]);
    shot(&orphan, egui::Theme::Light, "admin-spaces-page-no-owner", [WINDOW_SIZE[0], 1000.0]);
    // Archiving asks for the password in red.
    run(&mut page, &boss, vec![Event::Admin(AdminEvent::Ask(crate::admin::Action::ArchiveSpace { id: packaging, name: "Packaging".into() }))]);
    shot(&page, egui::Theme::Light, "admin-spaces-archive-confirm", [WINDOW_SIZE[0], 640.0]);
}

#[test]
fn the_admin_team_spaces_buttons_send_their_events() {
    let boss = spaces_agent();
    let mut h = harness(admin_spaces(&boss));
    h.run_steps(2);
    h.get_by_label("New team space").click();
    assert!(events_of(&mut h).contains(&Event::Admin(AdminEvent::NewSpace)));
    h.get_by_label("Find").click();
    assert!(events_of(&mut h).contains(&Event::Admin(AdminEvent::SearchSpaces)));
    assert_eq!(h.query_all_by_label("Manage").count(), 4);
    h.query_all_by_label("Manage").next().unwrap().click();
    assert!(events_of(&mut h).iter().any(|e| matches!(e, Event::Admin(AdminEvent::OpenSpace(_)))));
    // The switcher: Team spaces is its own segment between Storage and the audit log.
    h.get_by_label("Accounts").click();
    assert!(events_of(&mut h).contains(&Event::Admin(AdminEvent::Show(Section::Accounts))));
}
