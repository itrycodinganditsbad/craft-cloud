//! The filters of the activity tables (Recent activity on the overview, Security activity): a kind of event, a computer and a
//! text. They narrow the events that are loaded and ask the server for nothing, so a table that holds one page of the newest
//! events shows fewer of them, not older ones.

use craft_cloud_app::DeviceId;
use craft_cloud_proto::api::AuditEvent;

use crate::audit::action_label;
use crate::names::Names;

/// What the filters say about themselves, under the controls.
pub const HINT: &str = "Filters the loaded events";

/// A kind of event, by the group its name is in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ActivityKind {
    #[default]
    All,
    /// `auth.`: sign-ins, and the tries that failed.
    SignIn,
    /// `device.` and `join.`: computers signed out, joined and added.
    Computers,
    /// `account.`: password and two-factor changes.
    Account,
    /// `admin.` and `cli.`: what an admin or the server's command line did.
    Admin,
}

impl ActivityKind {
    /// The choices, in the order the menu lists them.
    pub const ALL: [ActivityKind; 5] = [Self::All, Self::SignIn, Self::Computers, Self::Account, Self::Admin];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All kinds",
            Self::SignIn => "Sign-in",
            Self::Computers => "Computers",
            Self::Account => "Account",
            Self::Admin => "Admin",
        }
    }

    /// Whether an event named `action` is of this kind.
    pub fn includes(self, action: &str) -> bool {
        let group = |prefixes: &[&str]| prefixes.iter().any(|p| action.starts_with(p));
        match self {
            Self::All => true,
            Self::SignIn => group(&["auth."]),
            Self::Computers => group(&["device.", "join."]),
            Self::Account => group(&["account."]),
            Self::Admin => group(&["admin.", "cli."]),
        }
    }
}

/// One edit of the filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterEdit {
    Kind(ActivityKind),
    /// A computer, or every computer.
    Computer(Option<DeviceId>),
    /// The search field, as typed.
    Search(String),
    /// Every filter off.
    Clear,
}

/// The filters of one activity table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityFilter {
    pub kind: ActivityKind,
    pub computer: Option<DeviceId>,
    /// What the event's words contain (any case).
    pub search: String,
}

impl ActivityFilter {
    /// Whether no filter is on.
    pub fn is_off(&self) -> bool {
        self.kind == ActivityKind::All && self.computer.is_none() && self.search.trim().is_empty()
    }

    pub fn edit(&mut self, edit: FilterEdit) {
        match edit {
            FilterEdit::Kind(kind) => self.kind = kind,
            FilterEdit::Computer(computer) => self.computer = computer,
            FilterEdit::Search(search) => self.search = search,
            FilterEdit::Clear => *self = Self::default(),
        }
    }

    /// Whether `event` passes all three filters.
    pub fn matches(&self, event: &AuditEvent) -> bool {
        if !self.kind.includes(&event.action) {
            return false;
        }
        if self.computer.is_some() && event.actor_device != self.computer {
            return false;
        }
        let needle = self.search.trim().to_lowercase();
        needle.is_empty() || action_label(&event.action).to_lowercase().contains(&needle)
    }

    /// The events that pass, in their order.
    pub fn visible<'a>(&self, events: &'a [AuditEvent]) -> Vec<&'a AuditEvent> {
        events.iter().filter(|e| self.matches(e)).collect()
    }
}

/// The computers the `events` name, each once, by the name `names` knows them under (then by ID), for the computer menu. The
/// chosen computer stays in the list even when no loaded event names it, so the menu can always show what is chosen.
pub fn computers(events: &[AuditEvent], chosen: Option<DeviceId>, names: &Names) -> Vec<(DeviceId, String)> {
    let mut found: Vec<DeviceId> = Vec::new();
    for device in events.iter().filter_map(|e| e.actor_device).chain(chosen) {
        if !found.contains(&device) {
            found.push(device);
        }
    }
    let mut named: Vec<(DeviceId, String)> = found.into_iter().map(|id| (id, names.computer(id).text)).collect();
    named.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()).then_with(|| a.0.to_string().cmp(&b.0.to_string())));
    named
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_cloud_app::Uuid;

    fn event(action: &str, device: Option<DeviceId>) -> AuditEvent {
        AuditEvent {
            id: Uuid::from_u128(1),
            at_ms: 0,
            actor_user: None,
            actor_device: device,
            action: action.into(),
            target_kind: String::new(),
            target_id: None,
            outcome: "success".into(),
            details: Default::default(),
            count: 1,
        }
    }

    #[test]
    fn every_action_of_the_api_is_of_exactly_one_kind_but_all() {
        for action in craft_cloud_app::AUDIT_ACTIONS {
            let kinds: Vec<_> = ActivityKind::ALL[1..].iter().filter(|k| k.includes(action)).collect();
            assert_eq!(kinds.len(), 1, "{action}: {kinds:?}");
            assert!(ActivityKind::All.includes(action));
        }
    }

    #[test]
    fn kind_computer_and_text_all_have_to_match() {
        let (a, b) = (DeviceId::generate(), DeviceId::generate());
        let events = vec![event("auth.login", Some(a)), event("auth.login", Some(b)), event("admin.role_changed", Some(a)), event("device.revoked", None)];
        let mut filter = ActivityFilter::default();
        assert!(filter.is_off());
        assert_eq!(filter.visible(&events).len(), 4);
        filter.edit(FilterEdit::Kind(ActivityKind::SignIn));
        assert_eq!(filter.visible(&events).len(), 2);
        filter.edit(FilterEdit::Computer(Some(a)));
        assert_eq!(filter.visible(&events).len(), 1);
        filter.edit(FilterEdit::Search("ROLE".into()));
        assert!(filter.visible(&events).is_empty(), "a sign-in isn't about a role");
        filter.edit(FilterEdit::Kind(ActivityKind::All));
        assert_eq!(filter.visible(&events).len(), 1);
        assert_eq!(filter.visible(&events).first().map(|e| e.action.as_str()), Some("admin.role_changed"));
        filter.edit(FilterEdit::Clear);
        assert!(filter.is_off());
        assert_eq!(filter.visible(&events).len(), 4);
    }

    #[test]
    fn the_search_reads_the_words_the_table_shows_and_ignores_blanks() {
        let events = vec![event("auth.login_failed", None), event("account.password_changed", None)];
        let mut filter = ActivityFilter { search: "  password ".into(), ..Default::default() };
        assert_eq!(filter.visible(&events).len(), 2, "the failed sign-in says wrong password");
        filter.search = "changed".into();
        assert_eq!(filter.visible(&events).len(), 1);
        filter.search = "   ".into();
        assert!(filter.is_off() && filter.visible(&events).len() == 2);
    }

    #[test]
    fn the_computer_menu_lists_each_computer_once_by_name_and_keeps_the_chosen_one() {
        let (a, b, gone) = (DeviceId::generate(), DeviceId::generate(), DeviceId::generate());
        let mut names = Names::default();
        names.learn_computers(&[device(a, "lptp1"), device(b, "BEAST")]);
        let events = vec![event("auth.login", Some(a)), event("auth.login", Some(b)), event("auth.login", Some(a)), event("auth.login", None)];
        let listed: Vec<String> = computers(&events, None, &names).into_iter().map(|(_, name)| name).collect();
        assert_eq!(listed, ["BEAST", "lptp1"]);
        let with_chosen = computers(&events, Some(gone), &names);
        assert_eq!(with_chosen.len(), 3);
        assert!(with_chosen.iter().any(|(id, _)| *id == gone));
    }

    fn device(id: DeviceId, name: &str) -> craft_cloud_app::DeviceInfo {
        craft_cloud_app::DeviceInfo {
            id,
            name: name.into(),
            created_ms: 0,
            last_used_ms: 0,
            revoked: false,
            current: false,
            client_kind: String::new(),
            platform: String::new(),
            app_version: String::new(),
            revoked_ms: None,
            revoke_reason: None,
        }
    }
}
