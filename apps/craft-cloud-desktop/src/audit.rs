//! The security audit log's pages, as a pure state like [`crate::admin`] (which hosts the admin's view of it as
//! [`View::Audit`]; the account's own activity is [`Screen::Activity`], open to every signed-in account).
//!
//! - **What is shown** is the server's: events newest first, a cursor given back exactly as it came, filters that are only
//!   what the server's contract allows (an action from the closed list [`AUDIT_ACTIONS`] or a prefix of it, an account the
//!   admin picked on its page). The server decides who reads what; a `forbidden` makes the app ask the account's role again.
//! - **The export** is the agent's stream of the server's NDJSON, written by the app straight to a file the person picked
//!   ([`Effect::AuditExport`], which writes to `<file>.part` and moves it into place only when the stream ended properly). It
//!   is never held in memory. Whole, or cut by the server at its limit (said so, not as a success), or failed (nothing kept).
//! - Answers that come late (a page asked for before the filter changed) match nothing: the numbers are [`crate::admin::fresh`].

use craft_cloud_app::{AuditPage, AuditQuery, Error, ExportOutcome, Uuid};
use craft_cloud_proto::api::AuditEvent;

use crate::admin::{AdminScreen, View, error_text, fresh, is_forbidden};
use crate::model::{Effect, Model, Overview, Screen};
use crate::names::{Named, Names};

/// Events on one page.
pub const PAGE_SIZE: u32 = 25;
/// The file name an export is offered under.
pub const EXPORT_NAME: &str = "craft-cloud-audit.ndjson";

/// A page of events with the cursors back to the first, and the call whose answer is wanted.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditList {
    pub events: Vec<AuditEvent>,
    /// How each page up to this one was asked for: `cursors[0]` is `None`, the last is this page's. The cursors are the
    /// server's, kept as they came.
    pub cursors: Vec<Option<String>>,
    /// The cursor of the (older) page after this one, if there is one.
    pub next: Option<String>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
    ticket: u64,
}

impl AuditList {
    pub fn new() -> Self {
        Self { events: Vec::new(), cursors: vec![None], next: None, loading: false, loaded: false, error: None, ticket: 0 }
    }

    /// The page number shown (from 1).
    pub fn page(&self) -> usize {
        self.cursors.len()
    }

    pub fn has_previous(&self) -> bool {
        self.cursors.len() > 1
    }

    /// Asks for the current page with the filters of `filter` (none for the account's own activity).
    fn load(&mut self, own: bool, filter: &AuditFilter) -> Effect {
        self.loading = true;
        self.error = None;
        self.ticket = fresh();
        let query = AuditQuery {
            cursor: self.cursors.last().cloned().flatten(),
            before_ms: None,
            limit: Some(PAGE_SIZE),
            user: filter.user.as_ref().map(|(id, _)| *id),
            action: filter.action.clone(),
        };
        Effect::AuditPage { own, query, ticket: self.ticket }
    }

    fn came(&mut self, ticket: u64, result: Result<AuditPage, Error>) -> Option<Error> {
        if ticket != self.ticket {
            return None;
        }
        self.loading = false;
        match result {
            Ok(page) => {
                self.loaded = true;
                self.error = None;
                self.events = page.events;
                self.next = page.next;
                None
            }
            Err(e) => {
                self.error = Some(error_text(&e));
                Some(e)
            }
        }
    }

    fn start_over(&mut self) {
        self.cursors = vec![None];
    }
}

impl Default for AuditList {
    fn default() -> Self {
        Self::new()
    }
}

/// What the admin's audit view is narrowed to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// An event name from [`AUDIT_ACTIONS`], or a prefix of them ending in a dot (`auth.`); none: every event.
    pub action: Option<String>,
    /// The account the events are about or by (its ID and name, for the label).
    pub user: Option<(Uuid, String)>,
}

/// Where an export is.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportState {
    Idle,
    /// The file picker is open (the call `ticket` is the one whose answer is wanted).
    Picking {
        ticket: u64,
    },
    /// The stream is being written to this file (the call `ticket` is the one whose answer is wanted).
    Running {
        path: String,
        ticket: u64,
    },
    /// Written: how many events, and whether the server cut it at its limit (then it is not the whole log).
    Done {
        path: String,
        outcome: ExportOutcome,
    },
    /// Nothing was kept.
    Failed(String),
}

/// The admin's audit view.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditView {
    pub list: AuditList,
    pub filter: AuditFilter,
    pub export: ExportState,
}

/// The account's own activity.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityScreen {
    /// The signed-in view the page came from and goes back to.
    pub overview: Overview,
    pub list: AuditList,
}

/// What happened on the audit pages: clicks and answers.
#[derive(Debug, Clone, PartialEq)]
pub enum AuditMsg {
    /// [`Effect::AuditPage`]'s answer (`own`: the account's own activity).
    Page {
        own: bool,
        ticket: u64,
        result: Result<AuditPage, Error>,
    },
    /// [`Effect::AuditExport`]'s answer.
    Exported {
        ticket: u64,
        result: Result<ExportOutcome, Error>,
    },

    /// The admin's audit view, for one account or all of them.
    OpenAudit(Option<(Uuid, String)>),
    /// Narrow to an event name or prefix of the closed list; `None`: every event.
    SetAction(Option<String>),
    /// Every account again.
    ClearUser,
    Next,
    Previous,
    Reload,
    /// Export what the filters say to a file (the person picks it).
    Export,
    /// The file picker's answer (`None`: cancelled).
    ExportPicked {
        ticket: u64,
        path: Option<String>,
    },
    /// The note about the last export was read.
    DismissExport,

    /// The signed-in account's own activity.
    OpenActivity,
    CloseActivity,
    ActivityNext,
    ActivityPrevious,
    ActivityReload,
}

impl Model {
    /// The account's own activity page, if shown.
    pub fn activity_screen(&self) -> Option<&ActivityScreen> {
        match &self.screen {
            Screen::Activity(a) => Some(a),
            _ => None,
        }
    }

    pub(crate) fn audit(&mut self, msg: AuditMsg) -> Vec<Effect> {
        match msg {
            AuditMsg::OpenActivity => {
                let Screen::SignedIn(overview) = &self.screen else { return Vec::new() };
                let mut screen = ActivityScreen { overview: overview.clone(), list: AuditList::new() };
                let effect = screen.list.load(true, &AuditFilter::default());
                self.screen = Screen::Activity(Box::new(screen));
                vec![effect]
            }
            AuditMsg::CloseActivity => {
                if let Screen::Activity(a) = &self.screen {
                    self.screen = Screen::SignedIn(a.overview.restored());
                }
                Vec::new()
            }
            AuditMsg::Page { own: true, ticket, result } => {
                let Screen::Activity(a) = &mut self.screen else { return Vec::new() };
                a.list.came(ticket, result);
                Vec::new()
            }
            AuditMsg::ActivityNext | AuditMsg::ActivityPrevious | AuditMsg::ActivityReload => {
                let Screen::Activity(a) = &mut self.screen else { return Vec::new() };
                let list = &mut a.list;
                let own = AuditFilter::default();
                match msg {
                    AuditMsg::ActivityNext => match list.next.clone() {
                        Some(next) if !list.loading => {
                            list.cursors.push(Some(next));
                            vec![list.load(true, &own)]
                        }
                        _ => Vec::new(),
                    },
                    AuditMsg::ActivityPrevious if list.has_previous() && !list.loading => {
                        list.cursors.pop();
                        vec![list.load(true, &own)]
                    }
                    AuditMsg::ActivityReload if !list.loading => vec![list.load(true, &own)],
                    _ => Vec::new(),
                }
            }
            msg => self.admin_audit(msg),
        }
    }

    fn admin_audit(&mut self, msg: AuditMsg) -> Vec<Effect> {
        let Screen::Admin(admin) = &mut self.screen else { return Vec::new() };
        admin.audit(msg)
    }
}

impl AdminScreen {
    fn audit_view(&mut self) -> Option<&mut AuditView> {
        match &mut self.view {
            View::Audit(view) => Some(view),
            _ => None,
        }
    }

    pub(crate) fn audit(&mut self, msg: AuditMsg) -> Vec<Effect> {
        let session = self.overview.session;
        match msg {
            AuditMsg::OpenAudit(user) => {
                if self.dialog.is_some() || self.code.is_some() {
                    return Vec::new();
                }
                let mut view = AuditView { list: AuditList::new(), filter: AuditFilter { action: None, user }, export: ExportState::Idle };
                let effect = view.list.load(false, &view.filter);
                self.view = View::Audit(Box::new(view));
                vec![effect]
            }
            AuditMsg::Page { own: false, ticket, result } => {
                let Some(view) = self.audit_view() else { return Vec::new() };
                match view.list.came(ticket, result) {
                    Some(e) if is_forbidden(&e) => vec![Effect::Account { session }],
                    _ => Vec::new(),
                }
            }
            AuditMsg::SetAction(action) => {
                let Some(view) = self.audit_view() else { return Vec::new() };
                // Only what the server's contract allows is ever asked for.
                if action.as_deref().is_some_and(|a| !craft_cloud_app::valid_audit_action(a)) {
                    return Vec::new();
                }
                view.filter.action = action;
                view.list.start_over();
                vec![view.list.load(false, &view.filter)]
            }
            AuditMsg::ClearUser => {
                let Some(view) = self.audit_view() else { return Vec::new() };
                view.filter.user = None;
                view.list.start_over();
                vec![view.list.load(false, &view.filter)]
            }
            AuditMsg::Next => {
                let Some(view) = self.audit_view() else { return Vec::new() };
                match view.list.next.clone() {
                    Some(next) if !view.list.loading => {
                        view.list.cursors.push(Some(next));
                        vec![view.list.load(false, &view.filter)]
                    }
                    _ => Vec::new(),
                }
            }
            AuditMsg::Previous => {
                let Some(view) = self.audit_view() else { return Vec::new() };
                if view.list.has_previous() && !view.list.loading {
                    view.list.cursors.pop();
                    vec![view.list.load(false, &view.filter)]
                } else {
                    Vec::new()
                }
            }
            AuditMsg::Reload => {
                let Some(view) = self.audit_view() else { return Vec::new() };
                if view.list.loading { Vec::new() } else { vec![view.list.load(false, &view.filter)] }
            }
            AuditMsg::Export => {
                let Some(view) = self.audit_view() else { return Vec::new() };
                if matches!(view.export, ExportState::Picking { .. } | ExportState::Running { .. }) {
                    return Vec::new();
                }
                // The answers of the picker and of the export carry this number: one that comes after the pages were left and
                // opened again, or for an earlier export, matches nothing.
                let ticket = fresh();
                view.export = ExportState::Picking { ticket };
                vec![Effect::PickExportFile { suggested: EXPORT_NAME.to_owned(), ticket }]
            }
            AuditMsg::ExportPicked { ticket, path: picked } => {
                let Some(view) = self.audit_view() else { return Vec::new() };
                if view.export != (ExportState::Picking { ticket }) {
                    return Vec::new();
                }
                match picked {
                    None => {
                        view.export = ExportState::Idle;
                        Vec::new()
                    }
                    Some(path) => {
                        view.export = ExportState::Running { path: path.clone(), ticket };
                        // Every event the filters say, not one page: no cursor, no limit.
                        let query = AuditQuery {
                            cursor: None,
                            before_ms: None,
                            limit: None,
                            user: view.filter.user.as_ref().map(|(id, _)| *id),
                            action: view.filter.action.clone(),
                        };
                        vec![Effect::AuditExport { query, path, ticket }]
                    }
                }
            }
            AuditMsg::Exported { ticket, result } => {
                let Some(view) = self.audit_view() else { return Vec::new() };
                let ExportState::Running { path, ticket: running } = &view.export else { return Vec::new() };
                if *running != ticket {
                    return Vec::new();
                }
                let path = path.clone();
                match result {
                    Ok(outcome) => {
                        view.export = ExportState::Done { path, outcome };
                        Vec::new()
                    }
                    Err(e) => {
                        let forbidden = is_forbidden(&e);
                        view.export = ExportState::Failed(error_text(&e));
                        if forbidden { vec![Effect::Account { session }] } else { Vec::new() }
                    }
                }
            }
            AuditMsg::DismissExport => {
                if let Some(view) = self.audit_view()
                    && !matches!(view.export, ExportState::Running { .. } | ExportState::Picking { .. })
                {
                    view.export = ExportState::Idle;
                }
                Vec::new()
            }
            // Handled by the model.
            AuditMsg::Page { own: true, .. }
            | AuditMsg::OpenActivity
            | AuditMsg::CloseActivity
            | AuditMsg::ActivityNext
            | AuditMsg::ActivityPrevious
            | AuditMsg::ActivityReload => Vec::new(),
        }
    }
}

/// What an event name says, in words; a name that isn't in the list (a newer server's) is shown as it is.
pub fn action_label(action: &str) -> &str {
    match action {
        "auth.login" => "Signed in",
        "auth.login_failed" => "Sign-in failed (wrong password)",
        "auth.second_factor_failed" => "Sign-in failed (wrong code)",
        "auth.recovery_code_used" => "Recovery code used",
        "auth.two_factor_locked" => "Two-factor sign-in locked after wrong codes",
        "device.revoked" => "Computer signed out",
        "device.revoked_others" => "Other computers signed out",
        "account.password_changed" => "Password changed",
        "account.two_factor_enabled" => "Two-factor sign-in turned on",
        "account.two_factor_disabled" => "Two-factor sign-in turned off",
        "account.recovery_codes_regenerated" => "New recovery codes made",
        "admin.user_created" => "Account created by an admin",
        "admin.user_disabled" => "Account disabled by an admin",
        "admin.user_enabled" => "Account enabled by an admin",
        "admin.user_removed" => "Account removed by an admin",
        "admin.role_changed" => "Role changed by an admin",
        "admin.quota_changed" => "Quota changed by an admin",
        // The name the compression setting's branch adds to the server's list; its text is here so that it doesn't show raw.
        "admin.compression_changed" => "Compression level changed",
        "admin.invite_issued" => "Invite made by an admin",
        "admin.invite_revoked" => "Invite or reset code revoked by an admin",
        "admin.password_reset_issued" => "Password reset code made by an admin",
        "admin.two_factor_reset" => "Two-factor sign-in turned off by an admin",
        "admin.device_revoked" => "Computer signed out by an admin",
        "join.invite_redeemed" => "Joined with an invite",
        "join.reset_redeemed" => "Joined with a password reset code",
        "join.pairing_redeemed" => "Computer added with a code",
        "cli.password_reset" => "Password reset on the server",
        "cli.two_factor_reset" => "Two-factor sign-in turned off on the server",
        "cli.device_revoked" => "Computer signed out on the server",
        "cli.devices_revoked_all" => "Every computer signed out on the server",
        "cli.user_removed" => "Account removed on the server",
        "space.created" => "Team space created",
        "space.renamed" => "Team space renamed",
        "space.archived" => "Team space archived",
        "space.unarchived" => "Team space brought back",
        "space.quota_changed" => "Team space limit changed",
        "space.member_invited" => "Invited to a team space",
        "space.member_joined" => "Joined a team space",
        "space.member_declined" => "Declined an invitation to a team space",
        "space.member_role_changed" => "Role in a team space changed",
        "space.member_removed" => "Removed from a team space",
        "space.member_left" => "Left a team space",
        "cli.space_created" => "Team space created on the server",
        "cli.space_member_changed" => "Team space member changed on the server",
        "cli.space_archived" => "Team space archived on the server",
        other => other,
    }
}

/// A filter choice's label: a prefix as a group, a name as its words.
pub fn filter_label(filter: &str) -> String {
    match filter {
        "auth." => "Sign-ins".to_owned(),
        "device." => "Computers signed out".to_owned(),
        "account." => "Account changes".to_owned(),
        "admin." => "Admin changes".to_owned(),
        "join." => "Joining".to_owned(),
        "space." => "Team spaces".to_owned(),
        "cli." => "Server command line".to_owned(),
        name => action_label(name).to_owned(),
    }
}

/// The groups of the filter, then every name: the closed list the server filters by.
pub fn filter_choices() -> Vec<&'static str> {
    let mut choices = vec!["auth.", "device.", "account.", "admin.", "join.", "space.", "cli."];
    choices.extend(craft_cloud_app::AUDIT_ACTIONS);
    choices
}

/// The first eight characters of an ID, to tell accounts and computers apart in a line (the whole ID is on hover).
pub fn short_id(id: impl std::fmt::Display) -> String {
    id.to_string().chars().filter(|c| *c != '-').take(8).collect()
}

/// Who did it and what it was about, in a line, and the same with the whole IDs for hover: the account that acted (or "command
/// line" for the server's own commands, "unknown" for a failed sign-in, where nobody proved who they were), the account or computer
/// it concerned, and the computer a sign-out or admin act names in its details. Accounts and computers `names` knows are shown by
/// name; the others by a short ID.
pub fn who_line(event: &AuditEvent, names: &Names) -> (String, String) {
    let mut line = Vec::new();
    let mut full = Vec::new();
    match (event.actor_user, event.action.starts_with("cli.")) {
        (Some(user), _) => {
            let who = names.account(user);
            line.push(format!("by {}", who.text));
            full.push(format!("by {}", who.full.replace('\n', " ")));
        }
        (None, true) => {
            line.push("by the command line".to_owned());
            full.push("by the server's command line".to_owned());
        }
        (None, false) => {
            line.push("by unknown".to_owned());
            full.push("by unknown: nobody had proved who they were".to_owned());
        }
    }
    if let Some(device) = event.actor_device {
        let computer = names.computer(device);
        line.push(format!("on computer {}", computer.text));
        full.push(format!("on {}", computer.full.replace('\n', " ")));
    }
    if let Some(target) = event.target_id {
        let kind = if event.target_kind.is_empty() || event.target_kind == "user" { "account" } else { event.target_kind.as_str() };
        let about = if kind == "account" { names.account(target) } else { names.computer(target) };
        let about = if kind == "account" || kind == "device" {
            about
        } else {
            Named { text: short_id(target), full: format!("{kind} {target}"), known: false }
        };
        line.push(format!("about {kind} {}", about.text));
        full.push(format!("about {}", about.full.replace('\n', " ")));
    }
    if let Some(device) = event.details.device {
        let computer = names.computer(device);
        line.push(format!("computer {}", computer.text));
        full.push(computer.full.replace('\n', " "));
    }
    (line.join(" · "), full.join("\n"))
}

/// The typed details of an event in a few words (nothing the server doesn't write from closed lists).
pub fn details_line(event: &AuditEvent) -> String {
    let d = &event.details;
    let mut parts: Vec<String> = Vec::new();
    if let Some(kind) = &d.client_kind {
        parts.push(format!("from {kind}"));
    }
    if let Some(platform) = &d.platform {
        parts.push(platform.clone());
    }
    if let Some(step) = &d.step {
        parts.push(format!("at {step}"));
    }
    if let (Some(from), Some(to)) = (&d.role_from, &d.role_to) {
        parts.push(format!("{from} to {to}"));
    }
    match &d.quota_bytes {
        Some(Some(bytes)) => parts.push(format!("quota {}", crate::admin::format_bytes(*bytes))),
        Some(None) => parts.push("quota removed".to_owned()),
        None => {}
    }
    if let Some(n) = d.revoked {
        parts.push(format!("{n} computer{} signed out", if n == 1 { "" } else { "s" }));
    }
    if d.two_factor_removed == Some(true) {
        parts.push("two-factor turned off".to_owned());
    }
    if let Some(reason) = &d.reason {
        parts.push(format!("reason: {reason}"));
    }
    if event.count > 1 {
        parts.push(format!("{} tries", event.count));
    }
    if event.outcome == "failure" && !parts.iter().any(|p| p.starts_with("at ")) {
        parts.push("failed".to_owned());
    }
    parts.join(" · ")
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use craft_cloud_proto::api::AuditDetails;

    use super::*;
    use crate::admin::AdminEvent;
    use crate::admin::tests as support;
    use crate::agent::fake::FakeAgent;
    use crate::agent::{Starter, call};
    use crate::model::Event;

    /// The picker's answer for the export that waits for it.
    pub(crate) fn picked(model: &mut Model, path: Option<&str>) -> Vec<Effect> {
        let View::Audit(view) = &model.admin_screen().unwrap().view else { panic!("not the audit view") };
        let ExportState::Picking { ticket } = view.export else { panic!("not picking: {:?}", view.export) };
        model.update(Event::Audit(AuditMsg::ExportPicked { ticket, path: path.map(str::to_owned) }))
    }

    /// The answer of the export that runs.
    pub(crate) fn finished(model: &mut Model, result: Result<ExportOutcome, Error>) {
        let View::Audit(view) = &model.admin_screen().unwrap().view else { panic!("not the audit view") };
        let ExportState::Running { ticket, .. } = view.export else { panic!("not running: {:?}", view.export) };
        model.update(Event::Audit(AuditMsg::Exported { ticket, result }));
    }

    fn on_audit(agent: &FakeAgent) -> Model {
        let mut model = support::on_the_pages(agent);
        support::run(&mut model, agent, vec![Event::Audit(AuditMsg::OpenAudit(None))]);
        model
    }

    fn view(model: &Model) -> &AuditView {
        match &model.admin_screen().unwrap().view {
            View::Audit(view) => view,
            other => panic!("not the audit view: {other:?}"),
        }
    }

    fn ev(msg: AuditMsg) -> Vec<Event> {
        vec![Event::Audit(msg)]
    }

    #[test]
    fn the_admins_audit_view_pages_with_the_servers_cursors() {
        let boss = support::agent(true, 1);
        boss.audit_events.lock().unwrap().extend((0..60).map(|n| FakeAgent::sample_event(n, "auth.login")));
        let mut model = on_audit(&boss);
        let v = view(&model);
        assert_eq!((v.list.page(), v.list.events.len(), v.list.next.as_deref()), (1, PAGE_SIZE as usize, Some("25")));
        assert!(model.update(Event::Audit(AuditMsg::Previous)).is_empty());
        support::run(&mut model, &boss, ev(AuditMsg::Next));
        assert_eq!(view(&model).list.page(), 2);
        // The cursor goes back exactly as it came.
        let asked = boss.audit_queries.lock().unwrap().last().cloned().unwrap();
        assert_eq!((asked.cursor.as_deref(), asked.limit), (Some("25"), Some(PAGE_SIZE)));
        support::run(&mut model, &boss, ev(AuditMsg::Next));
        assert_eq!((view(&model).list.events.len(), view(&model).list.next.clone()), (10, None));
        assert!(model.update(Event::Audit(AuditMsg::Next)).is_empty(), "no page after the last");
        support::run(&mut model, &boss, ev(AuditMsg::Previous));
        assert_eq!(view(&model).list.page(), 2);
    }

    #[test]
    fn filters_are_the_closed_list_and_start_the_list_over() {
        let boss = support::agent(true, 1);
        boss.audit_events
            .lock()
            .unwrap()
            .extend((0..30).map(|n| FakeAgent::sample_event(n, if n % 2 == 0 { "auth.login" } else { "admin.user_created" })));
        let mut model = on_audit(&boss);
        support::run(&mut model, &boss, ev(AuditMsg::Next));
        assert_eq!(view(&model).list.page(), 2);
        support::run(&mut model, &boss, ev(AuditMsg::SetAction(Some("admin.".into()))));
        assert_eq!(view(&model).list.page(), 1, "a filter starts over from the first page");
        assert!(view(&model).list.events.iter().all(|e| e.action.starts_with("admin.")));
        let asked = boss.audit_queries.lock().unwrap().last().cloned().unwrap();
        assert_eq!((asked.action.as_deref(), asked.cursor), (Some("admin."), None));
        // Anything that is not in the closed list (or a prefix of it) is never asked for.
        for bad in ["nope.", "auth.log", "x' OR 1=1", ""] {
            let before = boss.audit_queries.lock().unwrap().len();
            assert!(model.update(Event::Audit(AuditMsg::SetAction(Some(bad.into())))).is_empty(), "{bad:?}");
            assert_eq!(boss.audit_queries.lock().unwrap().len(), before);
        }
        assert_eq!(view(&model).filter.action.as_deref(), Some("admin."));
        support::run(&mut model, &boss, ev(AuditMsg::SetAction(None)));
        assert_eq!(view(&model).filter.action, None);
        // Every choice the page offers is one the server takes.
        assert!(filter_choices().iter().all(|c| craft_cloud_app::valid_audit_action(c)));
        assert_eq!(filter_choices().len(), 52);
        assert_eq!(action_label("auth.login"), "Signed in");
        assert_eq!(action_label("something.new"), "something.new");
        assert_eq!(filter_label("auth."), "Sign-ins");
    }

    /// Every name of the API's list is said in words, and the compression change, which the server writes once its setting is
    /// in, is too.
    #[test]
    fn every_audit_name_has_its_words() {
        for action in craft_cloud_app::AUDIT_ACTIONS {
            assert_ne!(action_label(action), action, "{action} shows raw");
        }
        assert_eq!(action_label("admin.compression_changed"), "Compression level changed");
        assert_eq!(action_label("space.member_invited"), "Invited to a team space");
        assert_eq!(filter_label("space."), "Team spaces");
    }

    #[test]
    fn an_account_filter_comes_from_its_page_and_is_cleared() {
        let boss = support::agent(true, 2);
        let mut model = support::on_the_pages(&boss);
        let ana = support::user_named(&model, "user01");
        support::run(&mut model, &boss, ev(AuditMsg::OpenAudit(Some((ana.id, "user01".into())))));
        let asked = boss.audit_queries.lock().unwrap().last().cloned().unwrap();
        assert_eq!(asked.user, Some(ana.id));
        assert_eq!(view(&model).filter.user.as_ref().map(|(_, n)| n.as_str()), Some("user01"));
        support::run(&mut model, &boss, ev(AuditMsg::ClearUser));
        assert_eq!(boss.audit_queries.lock().unwrap().last().unwrap().user, None);
        // Not from a password dialog's page.
        support::run(&mut model, &boss, vec![Event::Admin(AdminEvent::BackToList)]);
        let ask = crate::admin::Action::Disable { id: ana.id, username: "user01".into() };
        support::run(&mut model, &boss, vec![Event::Admin(AdminEvent::Ask(ask))]);
        assert!(model.update(Event::Audit(AuditMsg::OpenAudit(None))).is_empty());
    }

    #[test]
    fn a_page_that_comes_late_is_ignored_and_a_refusal_is_said() {
        let boss = support::agent(true, 1);
        boss.audit_events.lock().unwrap().extend((0..5).map(|n| FakeAgent::sample_event(n, "auth.login")));
        let mut model = support::on_the_pages(&boss);
        let first = model.update(Event::Audit(AuditMsg::OpenAudit(None)));
        let Some(Effect::AuditPage { ticket: old, .. }) = first.first().cloned() else { panic!("{first:?}") };
        // Another filter asks again while the first answer is still on its way.
        support::run(&mut model, &boss, ev(AuditMsg::SetAction(Some("auth.".into()))));
        let current = view(&model).list.events.clone();
        assert_eq!(current.len(), 5);
        let page = AuditPage { events: vec![FakeAgent::sample_event(99, "auth.login")], next: None };
        model.update(Event::Audit(AuditMsg::Page { own: false, ticket: old, result: Ok(page) }));
        assert_eq!(view(&model).list.events, current, "the first call's answer matches nothing");
        // A refusal is said; `forbidden` asks the account's role again.
        let forbidden = Error::Refused { code: "forbidden".into(), message: "Only admins".into(), name_taken: None };
        let Some(Effect::AuditPage { ticket, .. }) = model.update(Event::Audit(AuditMsg::Reload)).first().cloned() else { panic!() };
        let effects = model.update(Event::Audit(AuditMsg::Page { own: false, ticket, result: Err(forbidden) }));
        assert!(matches!(effects.as_slice(), [Effect::Account { .. }]), "{effects:?}");
        assert!(view(&model).list.error.as_deref().is_some_and(|e| e.starts_with("The server doesn't allow this")));
    }

    #[test]
    fn an_export_is_picked_run_and_whole_truncated_or_failed() {
        let boss = support::agent(true, 1);
        boss.audit_events.lock().unwrap().extend((0..3).map(|n| FakeAgent::sample_event(n, "auth.login")));
        let mut model = on_audit(&boss);
        // Cancelling the picker is nothing.
        let asked = model.update(Event::Audit(AuditMsg::Export));
        assert!(matches!(asked.as_slice(), [Effect::PickExportFile { suggested, .. }] if suggested == EXPORT_NAME), "{asked:?}");
        assert!(model.update(Event::Audit(AuditMsg::Export)).is_empty(), "one at a time");
        picked(&mut model, None);
        assert_eq!(view(&model).export, ExportState::Idle);
        // Whole.
        support::run(&mut model, &boss, ev(AuditMsg::SetAction(Some("auth.".into()))));
        model.update(Event::Audit(AuditMsg::Export));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("out.ndjson").display().to_string();
        let effects = picked(&mut model, Some(&file));
        let [Effect::AuditExport { query, path, .. }] = effects.as_slice() else { panic!("{effects:?}") };
        assert_eq!((path.as_str(), query.action.as_deref(), query.cursor.clone(), query.limit), (file.as_str(), Some("auth."), None, None));
        let start: Starter = Arc::new(|_: &dyn crate::agent::Agent| Err(String::new()));
        let answer = call(&boss, &start, effects.into_iter().next().unwrap()).unwrap();
        model.update(answer);
        assert_eq!(view(&model).export, ExportState::Done { path: file.clone(), outcome: ExportOutcome { events: 3, truncated: false } });
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 3);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temporary file is left");
        // Truncated: the file is the newest events and the page says it is not the whole log.
        *boss.export_truncated.lock().unwrap() = true;
        model.update(Event::Audit(AuditMsg::DismissExport));
        model.update(Event::Audit(AuditMsg::Export));
        let effects = picked(&mut model, Some(&file));
        let answer = call(&boss, &start, effects.into_iter().next().unwrap()).unwrap();
        model.update(answer);
        assert!(matches!(&view(&model).export, ExportState::Done { outcome, .. } if outcome.truncated), "{:?}", view(&model).export);
        // Failed (the stream broke): said, and no file of ours is left, and an older one, and a `.part` that is somebody else's,
        // are not touched.
        *boss.export_truncated.lock().unwrap() = false;
        std::fs::write(&file, "old").unwrap();
        let theirs = format!("{file}.part");
        std::fs::write(&theirs, "not ours").unwrap();
        boss.export_fails.lock().unwrap().push(Error::Io("the export broke off: reset".into()));
        model.update(Event::Audit(AuditMsg::DismissExport));
        model.update(Event::Audit(AuditMsg::Export));
        let effects = picked(&mut model, Some(&file));
        let answer = call(&boss, &start, effects.into_iter().next().unwrap()).unwrap();
        model.update(answer);
        assert!(matches!(&view(&model).export, ExportState::Failed(m) if m.contains("broke off")), "{:?}", view(&model).export);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old", "a failed export leaves what was there");
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "not ours", "a file called .part is not ours to truncate or delete");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2, "only those two files");
        // A whole export replaces the destination and still leaves the other file alone.
        model.update(Event::Audit(AuditMsg::DismissExport));
        model.update(Event::Audit(AuditMsg::Export));
        let effects = picked(&mut model, Some(&file));
        let answer = call(&boss, &start, effects.into_iter().next().unwrap()).unwrap();
        model.update(answer);
        assert_eq!(std::fs::read_to_string(&file).unwrap().lines().count(), 3);
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "not ours");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        // An answer when no export runs is ignored.
        model.update(Event::Audit(AuditMsg::DismissExport));
        model.update(Event::Audit(AuditMsg::Exported { ticket: 1, result: Ok(ExportOutcome { events: 1, truncated: false }) }));
        assert_eq!(view(&model).export, ExportState::Idle);
    }

    /// An export's answer belongs to the export that asked: leave the pages and open them again, start another, and the first one's
    /// answer (or its picker's) finishes nothing of the second.
    #[test]
    fn an_old_exports_answers_finish_nothing_of_a_newer_one() {
        let boss = support::agent(true, 1);
        boss.audit_events.lock().unwrap().extend((0..3).map(|n| FakeAgent::sample_event(n, "auth.login")));
        let mut model = on_audit(&boss);
        model.update(Event::Audit(AuditMsg::Export));
        let a = picked(&mut model, Some("a.ndjson"));
        let [Effect::AuditExport { ticket: a_ticket, .. }] = a.as_slice() else { panic!("{a:?}") };
        let a_ticket = *a_ticket;
        // Leave and come back, and start B (its picker open, then running).
        support::run(&mut model, &boss, vec![Event::Admin(AdminEvent::BackToList)]);
        support::run(&mut model, &boss, vec![Event::Audit(AuditMsg::OpenAudit(None))]);
        let asked = model.update(Event::Audit(AuditMsg::Export));
        let [Effect::PickExportFile { ticket: b_picker, .. }] = asked.as_slice() else { panic!("{asked:?}") };
        let b_picker = *b_picker;
        assert_ne!(a_ticket, b_picker);
        // A's answer arrives while B is still picking, and A's old picker answer arrives too.
        model.update(Event::Audit(AuditMsg::Exported { ticket: a_ticket, result: Ok(ExportOutcome { events: 3, truncated: false }) }));
        assert_eq!(view(&model).export, ExportState::Picking { ticket: b_picker });
        model.update(Event::Audit(AuditMsg::ExportPicked { ticket: a_ticket, path: Some("a.ndjson".into()) }));
        assert_eq!(view(&model).export, ExportState::Picking { ticket: b_picker }, "A's picker answer is not B's");
        let b = model.update(Event::Audit(AuditMsg::ExportPicked { ticket: b_picker, path: Some("b.ndjson".into()) }));
        assert!(matches!(b.as_slice(), [Effect::AuditExport { path, .. }] if path == "b.ndjson"), "{b:?}");
        // B runs; A's answer comes late and finishes nothing; B's own does.
        model.update(Event::Audit(AuditMsg::Exported { ticket: a_ticket, result: Err(Error::Io("the export broke off".into())) }));
        assert!(matches!(&view(&model).export, ExportState::Running { path, .. } if path == "b.ndjson"), "{:?}", view(&model).export);
        model.update(Event::Audit(AuditMsg::Exported { ticket: b_picker, result: Ok(ExportOutcome { events: 3, truncated: false }) }));
        assert!(matches!(&view(&model).export, ExportState::Done { path, .. } if path == "b.ndjson"), "{:?}", view(&model).export);
    }

    #[test]
    fn who_did_it_and_what_it_was_about_is_in_every_line() {
        let user = Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef);
        let other = Uuid::from_u128(0xfedc_ba98_7654_3210_fedc_ba98_7654_3210);
        let mut e = FakeAgent::sample_event(1, "admin.user_disabled");
        e.actor_user = Some(user);
        e.target_kind = "user".into();
        e.target_id = Some(other);
        let (line, full) = who_line(&e, &Names::default());
        assert_eq!(line, "by 01234567 · about account fedcba98");
        assert!(full.contains(&user.to_string()) && full.contains(&other.to_string()), "{full}");
        // A computer in the details; a computer that acted.
        let device = craft_cloud_app::DeviceId::generate();
        e.details.device = Some(device);
        e.actor_device = Some(device);
        let (line, full) = who_line(&e, &Names::default());
        assert!(line.contains(&format!("on computer {}", short_id(device))) && line.contains(&format!("computer {}", short_id(device))), "{line}");
        assert!(full.contains(&device.to_string()));
        // Nobody: the command line, or an unproven sign-in.
        let cli = FakeAgent::sample_event(2, "cli.password_reset");
        assert_eq!(who_line(&cli, &Names::default()).0, "by the command line");
        let failed = FakeAgent::sample_event(3, "auth.login_failed");
        assert_eq!(who_line(&failed, &Names::default()).0, "by unknown");
        assert!(who_line(&failed, &Names::default()).1.contains("nobody had proved"));
        // A target of another kind keeps its kind.
        let mut gone = FakeAgent::sample_event(4, "device.revoked");
        gone.actor_user = Some(user);
        gone.target_kind = "device".into();
        gone.target_id = Some(other);
        assert_eq!(who_line(&gone, &Names::default()).0, "by 01234567 · about device fedcba98");
    }

    #[test]
    fn known_accounts_and_computers_are_named_in_the_line_and_on_hover() {
        let user = Uuid::from_u128(1);
        let device = craft_cloud_app::DeviceId::generate();
        let mut names = Names::default();
        names.learn_account(user, "fabio");
        names.learn_computers(&[craft_cloud_app::DeviceInfo {
            id: device,
            name: "BEAST".into(),
            created_ms: 0,
            last_used_ms: 0,
            revoked: false,
            current: false,
            client_kind: String::new(),
            platform: String::new(),
            app_version: String::new(),
            revoked_ms: None,
            revoke_reason: None,
        }]);
        let mut e = FakeAgent::sample_event(1, "auth.login");
        e.actor_user = Some(user);
        e.actor_device = Some(device);
        let (line, full) = who_line(&e, &names);
        assert_eq!(line, "by fabio · on computer BEAST");
        assert!(full.contains(&user.to_string()) && full.contains(&device.to_string()), "{full}");
        // A computer nobody knows (removed) stays a short ID.
        let gone = craft_cloud_app::DeviceId::generate();
        e.actor_device = Some(gone);
        assert_eq!(who_line(&e, &names).0, format!("by fabio · on computer {}", short_id(gone)));
    }

    #[test]
    fn every_account_has_its_own_activity() {
        // A member: no admin pages, but the activity.
        let member = support::agent(false, 1);
        member.audit_events.lock().unwrap().extend((0..30).map(|n| FakeAgent::sample_event(n, "auth.login")));
        let mut model = support::signed_in(&member);
        assert!(!model.is_admin());
        let effects = model.update(Event::Audit(AuditMsg::OpenActivity));
        let [Effect::AuditPage { own: true, query, .. }] = effects.as_slice() else { panic!("{effects:?}") };
        assert_eq!((query.user, query.action.clone(), query.limit), (None, None, Some(PAGE_SIZE)));
        support::run(&mut model, &member, vec![]);
        let again = vec![Effect::AuditPage { own: true, query: query.clone(), ticket: model.activity_screen().unwrap().list.ticket }];
        let start: Starter = Arc::new(|_: &dyn crate::agent::Agent| Err(String::new()));
        let answer = call(&member, &start, again.into_iter().next().unwrap()).unwrap();
        model.update(answer);
        let a = model.activity_screen().unwrap();
        assert_eq!((a.list.events.len(), a.list.next.as_deref()), (PAGE_SIZE as usize, Some("25")));
        // Back to the signed-in view.
        model.update(Event::Audit(AuditMsg::CloseActivity));
        assert!(matches!(model.screen, Screen::SignedIn(_)));
        // Only from the signed-in view.
        let mut out = Model::new(crate::model::JoinDefaults::default(), true, String::new());
        assert!(out.update(Event::Audit(AuditMsg::OpenActivity)).is_empty());
    }

    #[test]
    fn events_are_told_in_words_without_anything_the_server_did_not_write() {
        let mut e = FakeAgent::sample_event(1, "admin.role_changed");
        e.details = AuditDetails { role_from: Some("member".into()), role_to: Some("admin".into()), ..AuditDetails::default() };
        assert_eq!(details_line(&e), "member to admin");
        e.details = AuditDetails { quota_bytes: Some(None), revoked: Some(2), ..AuditDetails::default() };
        assert_eq!(details_line(&e), "quota removed · 2 computers signed out");
        let mut failed = FakeAgent::sample_event(2, "auth.login_failed");
        failed.outcome = "failure".into();
        failed.count = 4;
        assert_eq!(details_line(&failed), "4 tries · failed");
    }
}
