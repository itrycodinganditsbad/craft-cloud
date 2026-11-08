//! The Shared page's state, as a pure function of what happened, like [`crate::model`] (which hosts it: the page is
//! [`Screen::Shared`], and [`Event::Spaces`] and the [`Effect::Spaces`] calls go through the same [`Model::update`]).
//!
//! - **What it shows**: the account's team spaces with their role, whether they sync on this computer, where, how that goes
//!   and what they store against their limit; the invitations at the top, each with Accept and Decline. The personal space is
//!   not here (the overview is its page).
//! - **An accepted space doesn't sync until the person says so** (spec 5.3): Accept offers "Sync to this computer", with the
//!   folder the agent would use (under `Craft Cloud Shared`, beside the personal folder) and a choice of another one.
//!   "Not now" leaves it off. A folder that already has files asks again before it is used.
//! - **A space's page**: its files summary and folder, the sync switch, banners for a read-only role, an archived space and
//!   lost access (the local copy is kept), held deletions with "Approve deletes", and its members.
//! - **Roles are the server's**: the page hides what a role may not do (only an owner sees the owner-only controls), and the
//!   server refuses it again if the role changed meanwhile; its words are shown.
//! - **Agent too old**: every call an agent from before these calls answers with `agent_too_old` ends in one message
//!   ([`TOO_OLD`]) instead of a list of errors.
//! - **Answers** carry the number of the call they answer ([`SharedScreen::ticket`]); an answer for an earlier call, or for a
//!   page that was left, is dropped.

use craft_cloud_app::{
    AddSpaceMember, ApproveDeletesAnswer, Error, SpaceConfirm, SpaceId, SpaceInfo, SpaceList, SpaceMember, SpaceRole, SpaceRun, SpaceSync,
    SpaceSyncRequest, SpaceUsage, Uuid, codes,
};

use crate::admin::{Room, error_text, fresh};
use crate::model::{Effect, Event, Model, Overview, Screen};

/// What every team call of an agent from before team spaces ends in.
pub const TOO_OLD: &str = "Update Craft Cloud on this computer to use team spaces";
/// The folder under which team spaces get their folders by default, beside the personal one.
pub const SHARED_FOLDER: &str = "Craft Cloud Shared";
/// A name for the account to invite is typed in this field of the members' part.
pub const INVITE_EMPTY: &str = "Type the account name of the person to invite";
/// The server's limit for an account name (and the admin pages').
const MAX_ACCOUNT_NAME: usize = 64;
/// What the page says when a role may not see a part.
pub const READ_ONLY_BANNER: &str = "You can read this space but not change it. What you change here stays on this computer and doesn't go up.";
/// What the page says of an archived space.
pub const ARCHIVED_BANNER: &str = "This space is archived: everyone, owners included, can only read it. Nothing is deleted.";
/// What the page says of a space the account lost access to.
pub const NO_ACCESS_BANNER: &str = "You no longer have access to this space. Your local copy is kept as it is and no longer syncs.";

/// What a change of a space's sync is told while the space is still being closed after an earlier change (the agent's `409`).
pub const CHANGING: &str = "This space is being changed. Try again in a moment.";

/// Whether the agent refused a change because the space is still being changed (a `409 conflict`).
pub fn is_changing(e: &Error) -> bool {
    matches!(e, Error::Refused { code, message, .. } if code == "conflict" && message.contains("being changed"))
}

/// The words of a call that failed: the agent's refusal as the admin pages say it, or [`TOO_OLD`] for an old agent.
fn words(e: &Error) -> String {
    if is_too_old(e) { TOO_OLD.to_owned() } else { error_text(e) }
}

/// Whether the agent answered that it doesn't know a team call.
pub fn is_too_old(e: &Error) -> bool {
    e.code() == Some(codes::AGENT_TOO_OLD)
}

/// How a space looks in a row or a banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    Good,
    Warn,
    Bad,
    Neutral,
}

/// A short word for a role ("Owner", "Editor", "Viewer").
pub fn role_label(role: SpaceRole) -> &'static str {
    match role {
        SpaceRole::Owner => "Owner",
        SpaceRole::Editor => "Editor",
        SpaceRole::Viewer => "Viewer",
    }
}

/// The role as a lower-case word in a sentence.
pub fn role_word(role: SpaceRole) -> &'static str {
    role.as_str()
}

/// The tags that say how a team space stands on this computer, most important first: `No access` (the account lost it),
/// `Unavailable` (the server needs an update), `Archived`, `Read only`, then how the sync goes (`Syncing`, `Synced`,
/// `Retrying`, `Paused`), or `Not syncing here` for a space that is off. An invitation has none.
pub fn state_tags(info: &SpaceInfo) -> Vec<(&'static str, Look)> {
    if info.state == "invited" {
        return Vec::new();
    }
    let mut tags = Vec::new();
    match &info.run_state {
        Some(SpaceRun::NoAccess { .. }) => return vec![("No access", Look::Bad)],
        Some(SpaceRun::Unavailable { .. }) => return vec![("Unavailable", Look::Warn)],
        _ => {}
    }
    if info.archived {
        tags.push(("Archived", Look::Neutral));
    }
    if info.read_only || info.role == SpaceRole::Viewer {
        tags.push(("Read only", Look::Warn));
    }
    match (info.sync, &info.run_state) {
        (SpaceSync::Off, _) => tags.push(("Not syncing here", Look::Neutral)),
        (SpaceSync::Unavailable, _) => tags.push(("Unavailable", Look::Warn)),
        (SpaceSync::On, Some(SpaceRun::Syncing)) => tags.push(("Syncing", Look::Good)),
        (SpaceSync::On, Some(SpaceRun::Retrying { .. })) => tags.push(("Retrying", Look::Warn)),
        (SpaceSync::On, Some(SpaceRun::Paused { .. })) => tags.push(("Needs you", Look::Bad)),
        (SpaceSync::On, Some(SpaceRun::PausedByUser)) => tags.push(("Paused", Look::Neutral)),
        (SpaceSync::On, _) => tags.push(("Synced", Look::Good)),
    }
    tags
}

/// The one line under a space's name in the list: where it syncs, or that it doesn't.
pub fn folder_line(info: &SpaceInfo) -> String {
    match (&info.folder, info.sync) {
        (Some(folder), SpaceSync::On) => folder.clone(),
        (Some(_), _) => "Not syncing to this computer".to_owned(),
        (None, _) => String::new(),
    }
}

/// The name of a space for people: its name, or "Team space" for a server that didn't say.
pub fn space_name(info: &SpaceInfo) -> String {
    info.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| "Team space".to_owned())
}

/// What a space stores, as the page asked for it.
#[derive(Debug, Clone, PartialEq)]
pub enum Used {
    /// Asked for, not answered yet.
    Loading,
    Known(SpaceUsage),
    /// The server couldn't say (or the account may not ask).
    Failed,
}

impl Used {
    /// The share of the limit, when there is a limit.
    pub fn room(&self) -> Option<Room> {
        match self {
            Used::Known(usage) => usage.quota_bytes.map(|total| Room { used: usage.stored_bytes, total }),
            _ => None,
        }
    }

    /// `1.2 GiB of 10 GiB`, `1.2 GiB` without a limit, a dash when it isn't known.
    pub fn text(&self) -> String {
        match self {
            Used::Known(usage) => match usage.quota_bytes {
                Some(limit) => format!("{} of {}", crate::admin::format_bytes(usage.stored_bytes), crate::admin::format_bytes(limit)),
                None => crate::admin::format_bytes(usage.stored_bytes),
            },
            Used::Loading => "…".to_owned(),
            Used::Failed => "–".to_owned(),
        }
    }
}

/// The offer to sync a space on this computer, after Accept or from the space's page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOffer {
    pub space: SpaceId,
    pub name: String,
    /// The folder it would sync to: the agent's default (as the list shows it) until another is chosen.
    pub folder: Option<String>,
    /// `folder` was chosen here and is sent; otherwise the agent uses its own.
    pub chosen: bool,
    /// The folder has files in it: sync it anyway (the files that differ become conflict copies, nothing is deleted).
    pub adopt: bool,
    /// The agent said the folder has files; the person is asked.
    pub asks_adopt: bool,
    /// The agent said the space is still being changed: the same click tries again.
    pub retry: bool,
    /// The offer comes from an invitation just accepted.
    pub after_accept: bool,
    pub busy: bool,
    pub error: Option<String>,
}

/// What a call of the page is doing now; one at a time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Doing {
    Accepting(SpaceId),
    Declining(SpaceId),
    Syncing(SpaceId),
    Inviting,
    ChangingRole(Uuid),
    Removing(Uuid),
    Leaving,
    Renaming,
    CheckingDeletes,
    ApprovingDeletes,
}

/// Held deletions of a team space, as the agent listed them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Deletes {
    #[default]
    Idle,
    /// Nothing is held back (said after a look).
    NoneHeld,
    /// These are held back, as the set `set` the agent named: the question "approve them?" is open. Only that set is approved.
    Asking { set: String, lines: Vec<String> },
    /// Approved: the agent carries them out in a moment.
    Approved(usize),
}

/// The text field and the role of "Invite by account name".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteForm {
    pub name: String,
    pub role: SpaceRole,
    pub error: Option<String>,
}

impl Default for InviteForm {
    fn default() -> Self {
        Self { name: String::new(), role: SpaceRole::Editor, error: None }
    }
}

/// The members of one space.
#[derive(Debug, Clone, PartialEq)]
pub struct Members {
    pub list: Option<Vec<SpaceMember>>,
    pub loading: bool,
    pub error: Option<String>,
}

/// One space's page.
#[derive(Debug, Clone, PartialEq)]
pub struct SpacePage {
    pub space: SpaceId,
    pub members: Members,
    /// The name field while it is being edited.
    pub rename: Option<String>,
    pub invite: InviteForm,
    /// The member a removal is asked for.
    pub removing: Option<Uuid>,
    /// "Leave this space?" is open.
    pub leaving: bool,
    pub deletes: Deletes,
    /// What went wrong with the last action of the page, for people.
    pub error: Option<String>,
}

impl SpacePage {
    fn new(space: SpaceId) -> Self {
        Self {
            space,
            members: Members { list: None, loading: false, error: None },
            rename: None,
            invite: InviteForm::default(),
            removing: None,
            leaving: false,
            deletes: Deletes::Idle,
            error: None,
        }
    }
}

/// Which page of the Shared tab is shown.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    List,
    Space(Box<SpacePage>),
}

/// The Shared tab.
#[derive(Debug, Clone, PartialEq)]
pub struct SharedScreen {
    /// The signed-in view the tab came from and goes back to.
    pub overview: Overview,
    /// The spaces as the agent listed them, once it did (the personal space left out).
    pub spaces: Option<Vec<SpaceInfo>>,
    pub loading: bool,
    /// Why the list couldn't be had.
    pub error: Option<String>,
    /// The agent is from before team spaces: the page says so and offers nothing.
    pub too_old: bool,
    /// What each space stores, as it was asked for.
    pub used: Vec<(SpaceId, Used)>,
    pub view: View,
    pub offer: Option<SyncOffer>,
    pub doing: Option<Doing>,
    /// What "Try again" sends, after a change was refused as still under way.
    pub retry: Option<SpacesMsg>,
    /// What went wrong with the last action on the list, for people.
    pub error_action: Option<String>,
    /// What the last action did, for people (a small note under the title).
    pub notice: Option<String>,
    /// The number of the call whose answer is wanted.
    pub ticket: u64,
    list_ticket: u64,
    members_ticket: u64,
}

impl SharedScreen {
    fn new(overview: Overview) -> Self {
        Self {
            overview,
            spaces: None,
            loading: false,
            error: None,
            too_old: false,
            used: Vec::new(),
            view: View::List,
            offer: None,
            doing: None,
            retry: None,
            error_action: None,
            notice: None,
            ticket: 0,
            list_ticket: 0,
            members_ticket: 0,
        }
    }

    /// The team spaces that are invitations, in the order the server listed them.
    pub fn invitations(&self) -> Vec<&SpaceInfo> {
        self.spaces.iter().flatten().filter(|s| s.state == "invited").collect()
    }

    /// The team spaces the account is a member of, by name.
    pub fn joined(&self) -> Vec<&SpaceInfo> {
        self.spaces.iter().flatten().filter(|s| s.state != "invited").collect()
    }

    pub fn find(&self, space: SpaceId) -> Option<&SpaceInfo> {
        self.spaces.iter().flatten().find(|s| s.space == space)
    }

    pub fn usage_of(&self, space: SpaceId) -> Used {
        self.used.iter().find(|(id, _)| *id == space).map(|(_, used)| used.clone()).unwrap_or(Used::Failed)
    }

    /// The page of the space shown, if one is.
    pub fn page(&self) -> Option<&SpacePage> {
        match &self.view {
            View::Space(page) => Some(page),
            View::List => None,
        }
    }

    pub fn page_mut(&mut self) -> Option<&mut SpacePage> {
        match &mut self.view {
            View::Space(page) => Some(page),
            View::List => None,
        }
    }

    /// A new call's number; its answer is the only one wanted.
    fn next_ticket(&mut self) -> u64 {
        self.ticket = fresh();
        self.ticket
    }

    /// Whether the account is an owner of the shown space (the owner-only controls are shown).
    pub fn is_owner(&self, space: SpaceId) -> bool {
        self.find(space).is_some_and(|s| s.role == SpaceRole::Owner && !s.run_state_is_lost())
    }
}

trait Lost {
    fn run_state_is_lost(&self) -> bool;
}

impl Lost for SpaceInfo {
    fn run_state_is_lost(&self) -> bool {
        matches!(self.run_state, Some(SpaceRun::NoAccess { .. }))
    }
}

/// The calls of the page ([`Effect::Spaces`]); each answers with the [`SpacesMsg`] of the same name.
#[derive(Debug, Clone, PartialEq)]
pub enum SpacesCall {
    List {
        ticket: u64,
    },
    Usage {
        space: SpaceId,
        ticket: u64,
    },
    Accept {
        space: SpaceId,
        ticket: u64,
    },
    Decline {
        space: SpaceId,
        ticket: u64,
    },
    Sync {
        space: SpaceId,
        request: SpaceSyncRequest,
        ticket: u64,
    },
    Members {
        space: SpaceId,
        ticket: u64,
    },
    AddMember {
        space: SpaceId,
        request: AddSpaceMember,
        ticket: u64,
    },
    ChangeMember {
        space: SpaceId,
        user: Uuid,
        role: SpaceRole,
        ticket: u64,
    },
    RemoveMember {
        space: SpaceId,
        user: Uuid,
        ticket: u64,
    },
    Leave {
        space: SpaceId,
        ticket: u64,
    },
    Rename {
        space: SpaceId,
        name: String,
        ticket: u64,
    },
    /// Lists what is held back. It names a set that is never waiting, so the agent approves nothing (it lists the waiting
    /// set instead and says its name).
    LookAtDeletes {
        space: SpaceId,
        ticket: u64,
    },
    /// Approves exactly the set `set` the look named, if it is still the one waiting.
    ApproveDeletes {
        space: SpaceId,
        set: String,
        ticket: u64,
    },
}

/// What happened on the Shared tab: clicks, and the answers of its calls.
#[derive(Debug, Clone, PartialEq)]
pub enum SpacesMsg {
    // Answers.
    Listed {
        ticket: u64,
        result: Result<SpaceList, Error>,
    },
    Usage {
        space: SpaceId,
        ticket: u64,
        result: Result<SpaceUsage, Error>,
    },
    Accepted {
        space: SpaceId,
        ticket: u64,
        result: Result<(), Error>,
    },
    Declined {
        space: SpaceId,
        ticket: u64,
        result: Result<(), Error>,
    },
    SyncSet {
        space: SpaceId,
        ticket: u64,
        result: Result<SpaceInfo, Error>,
    },
    Members {
        space: SpaceId,
        ticket: u64,
        result: Result<Vec<SpaceMember>, Error>,
    },
    MemberAdded {
        space: SpaceId,
        ticket: u64,
        result: Result<SpaceMember, Error>,
    },
    MemberChanged {
        space: SpaceId,
        ticket: u64,
        result: Result<SpaceMember, Error>,
    },
    MemberRemoved {
        space: SpaceId,
        ticket: u64,
        result: Result<(), Error>,
    },
    Left {
        space: SpaceId,
        ticket: u64,
        result: Result<(), Error>,
    },
    Renamed {
        space: SpaceId,
        ticket: u64,
        result: Result<String, Error>,
    },
    Deletes {
        space: SpaceId,
        ticket: u64,
        result: Result<ApproveDeletesAnswer, Error>,
    },
    /// The folder picker's answer for the offer (`None`: cancelled).
    FolderPicked(Option<String>),

    // Clicks.
    /// The Shared tab.
    Open,
    /// Back to the signed-in view (another tab).
    Close,
    Reload,
    OpenSpace(SpaceId),
    BackToList,
    Accept(SpaceId),
    Decline(SpaceId),
    /// "Sync on this computer" on a space that is off: opens the offer.
    SyncOn(SpaceId),
    /// The switch off: stops syncing the space here; the folder and its files stay.
    SyncOff(SpaceId),
    /// "Choose another folder…" in the offer.
    OfferChoose,
    /// "Sync" in the offer.
    OfferSync,
    /// "Sync anyway": the chosen folder has files.
    OfferAdopt,
    /// "Not now".
    OfferSkip,
    OpenFolder(SpaceId),
    StartRename,
    SaveRename,
    CancelRename,
    SubmitInvite,
    SetRole {
        user: Uuid,
        role: SpaceRole,
    },
    AskRemove(Uuid),
    ConfirmRemove,
    CancelRemove,
    AskLeave,
    ConfirmLeave,
    CancelLeave,
    /// "Approve deletes": looks at what is held back.
    CheckDeletes,
    /// Approve what the look listed.
    ConfirmDeletes,
    CancelDeletes,
    DismissNotice,
    /// "Try again" after a change was refused because the space was still being changed.
    Retry,
}

impl From<SpacesMsg> for Event {
    fn from(msg: SpacesMsg) -> Self {
        Event::Spaces(msg)
    }
}

/// The deletions an agent's `approve-deletes` answer lists, one per line (`delete on the server: path`, `delete here: path`).
pub fn held_deletions(message: &str) -> Vec<String> {
    message
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("delete on the server:") || line.starts_with("delete here:"))
        .map(str::to_owned)
        .collect()
}

/// The set a listing of held deletions names (`waiting for approval, set 3f9a0b1c (check the list…`), if it names one.
pub fn waiting_set(message: &str) -> Option<String> {
    let rest = message.split_once("waiting for approval, set ")?.1;
    let set: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (!set.is_empty()).then_some(set)
}

/// The set name a look at the held deletions sends: no set is ever called that, so the agent lists and approves nothing.
pub const LOOK: &str = "look";

/// The number of invitations in a list.
pub fn count_invitations(list: &SpaceList) -> usize {
    list.spaces.iter().filter(|s| s.kind == "team" && s.state == "invited").count()
}

/// The role a typed account name and the picked role make, or what is wrong with the name.
fn invitation(form: &InviteForm) -> Result<AddSpaceMember, &'static str> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err(INVITE_EMPTY);
    }
    if name.len() > MAX_ACCOUNT_NAME || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) {
        return Err(crate::admin::BAD_USERNAME);
    }
    Ok(AddSpaceMember { username: name.to_owned(), role: form.role, confirm: SpaceConfirm::default() })
}

impl Model {
    /// The Shared tab, if shown.
    pub fn shared_screen(&self) -> Option<&SharedScreen> {
        match &self.screen {
            Screen::Shared(shared) => Some(shared),
            _ => None,
        }
    }

    /// The effect that asks the agent for the spaces: the invitations' count wherever the signed-in view is shown.
    pub(crate) fn ask_spaces(&mut self) -> Vec<Effect> {
        match &mut self.screen {
            Screen::Shared(shared) => {
                shared.loading = true;
                shared.list_ticket = fresh();
                vec![Effect::Spaces(SpacesCall::List { ticket: shared.list_ticket })]
            }
            _ => match self.overview() {
                Some(_) => vec![Effect::Spaces(SpacesCall::List { ticket: 0 })],
                None => Vec::new(),
            },
        }
    }

    pub(crate) fn spaces(&mut self, msg: SpacesMsg) -> Vec<Effect> {
        match msg {
            SpacesMsg::Open => self.open_shared(),
            SpacesMsg::Close => {
                if let Screen::Shared(shared) = &self.screen {
                    self.screen = Screen::SignedIn(shared.overview.restored());
                }
                Vec::new()
            }
            SpacesMsg::Listed { ticket, result } => self.listed(ticket, result),
            msg => {
                let Screen::Shared(shared) = &mut self.screen else { return Vec::new() };
                shared.update(msg)
            }
        }
    }

    fn open_shared(&mut self) -> Vec<Effect> {
        let Screen::SignedIn(overview) = &self.screen else { return Vec::new() };
        let shared = SharedScreen::new(overview.clone());
        self.screen = Screen::Shared(Box::new(shared));
        self.ask_spaces()
    }

    /// The agent's list: it sets the count of invitations wherever the signed-in view is, and the Shared tab's own list.
    fn listed(&mut self, ticket: u64, result: Result<SpaceList, Error>) -> Vec<Effect> {
        if let Screen::Shared(shared) = &mut self.screen {
            if ticket != shared.list_ticket {
                // The overview's own look, or an older one.
                if ticket == 0
                    && let Ok(list) = &result
                {
                    shared.overview.invitations = count_invitations(list);
                }
                return Vec::new();
            }
            return shared.list_came(result);
        }
        if let Ok(list) = &result
            && let Some(overview) = self.overview_mut()
        {
            overview.invitations = count_invitations(list);
        }
        Vec::new()
    }
}

impl SharedScreen {
    /// Marks the screen as having no team spaces to show: the agent is too old.
    fn too_old(&mut self) {
        self.too_old = true;
        self.loading = false;
        self.doing = None;
        self.offer = None;
        self.error = None;
        self.error_action = None;
    }

    fn list_came(&mut self, result: Result<SpaceList, Error>) -> Vec<Effect> {
        self.loading = false;
        match result {
            Ok(list) => {
                self.too_old = false;
                self.error = None;
                self.overview.invitations = count_invitations(&list);
                let team: Vec<SpaceInfo> = list.spaces.into_iter().filter(|s| s.kind == "team").collect();
                // An offer that waits for the agent's default folder takes it from the list.
                if let Some(offer) = &mut self.offer
                    && !offer.chosen
                    && let Some(info) = team.iter().find(|s| s.space == offer.space)
                {
                    offer.folder = info.folder.clone();
                    offer.name = space_name(info);
                }
                // A page of a space that is gone from the list goes back to the list.
                if let View::Space(page) = &self.view
                    && !team.iter().any(|s| s.space == page.space)
                {
                    self.view = View::List;
                }
                let mut effects = Vec::new();
                for info in team.iter().filter(|s| s.state != "invited" && !matches!(s.run_state, Some(SpaceRun::NoAccess { .. }))) {
                    if !self.used.iter().any(|(id, _)| *id == info.space) {
                        self.used.push((info.space, Used::Loading));
                    }
                    effects.push(Effect::Spaces(SpacesCall::Usage { space: info.space, ticket: self.list_ticket }));
                }
                self.spaces = Some(team);
                effects
            }
            Err(e) if is_too_old(&e) => {
                self.too_old();
                Vec::new()
            }
            Err(e) => {
                self.error = Some(words(&e));
                Vec::new()
            }
        }
    }

    /// The effect that asks for the list again.
    fn reload(&mut self) -> Effect {
        self.loading = true;
        self.list_ticket = fresh();
        Effect::Spaces(SpacesCall::List { ticket: self.list_ticket })
    }

    /// Whether an answer numbered `ticket` is the one wanted (and ends what the page was doing).
    fn answers(&mut self, ticket: u64) -> bool {
        if ticket != self.ticket || self.doing.is_none() {
            return false;
        }
        self.doing = None;
        true
    }

    fn update(&mut self, msg: SpacesMsg) -> Vec<Effect> {
        match msg {
            SpacesMsg::Open | SpacesMsg::Close | SpacesMsg::Listed { .. } => Vec::new(),
            SpacesMsg::Usage { space, ticket, result } => {
                // Asked with the list's number: an answer for an older list is dropped.
                if ticket != self.list_ticket {
                    return Vec::new();
                }
                let used = match result {
                    Ok(usage) => Used::Known(usage),
                    Err(e) => {
                        if is_too_old(&e) {
                            self.too_old();
                        }
                        Used::Failed
                    }
                };
                match self.used.iter_mut().find(|(id, _)| *id == space) {
                    Some(entry) => entry.1 = used,
                    None => self.used.push((space, used)),
                }
                Vec::new()
            }
            SpacesMsg::Reload => {
                if self.loading {
                    return Vec::new();
                }
                self.error_action = None;
                self.notice = None;
                let mut effects = vec![self.reload()];
                if let Some(space) = self.page().map(|p| p.space) {
                    effects.push(self.ask_members(space));
                }
                effects
            }
            SpacesMsg::OpenSpace(space) => {
                if self.find(space).is_none() || self.doing.is_some() {
                    return Vec::new();
                }
                self.view = View::Space(Box::new(SpacePage::new(space)));
                self.error_action = None;
                self.notice = None;
                let joined = self.find(space).is_some_and(|s| s.state != "invited");
                if !joined {
                    return Vec::new();
                }
                vec![self.ask_members(space)]
            }
            SpacesMsg::BackToList => {
                if self.doing.is_none() {
                    self.view = View::List;
                    self.offer = None;
                }
                Vec::new()
            }
            SpacesMsg::DismissNotice => {
                self.notice = None;
                Vec::new()
            }
            SpacesMsg::Retry => match self.retry.take() {
                Some(again) if self.doing.is_none() => self.update(again),
                _ => Vec::new(),
            },
            SpacesMsg::Accept(space) => self.accept(space),
            SpacesMsg::Accepted { space, ticket, result } => self.accepted(space, ticket, result),
            SpacesMsg::Decline(space) => {
                if self.doing.is_some() || !self.invitations().iter().any(|s| s.space == space) {
                    return Vec::new();
                }
                self.error_action = None;
                self.doing = Some(Doing::Declining(space));
                let ticket = self.next_ticket();
                vec![Effect::Spaces(SpacesCall::Decline { space, ticket })]
            }
            SpacesMsg::Declined { space, ticket, result } => {
                if !self.answers(ticket) {
                    return Vec::new();
                }
                let name = self.find(space).map(space_name).unwrap_or_default();
                match result {
                    Ok(()) => {
                        self.notice = Some(format!("You declined the invitation to {name}."));
                        vec![self.reload()]
                    }
                    Err(e) => self.action_failed(e),
                }
            }
            SpacesMsg::SyncOn(space) => self.start_offer(space),
            SpacesMsg::SyncOff(space) => {
                if self.doing.is_some() || self.find(space).is_none() {
                    return Vec::new();
                }
                self.error_action = None;
                self.doing = Some(Doing::Syncing(space));
                let ticket = self.next_ticket();
                vec![Effect::Spaces(SpacesCall::Sync { space, request: SpaceSyncRequest { on: false, folder: None, adopt: false }, ticket })]
            }
            SpacesMsg::OfferChoose => {
                let Some(offer) = &self.offer else { return Vec::new() };
                if offer.busy {
                    return Vec::new();
                }
                let start = offer.folder.clone().unwrap_or_default();
                vec![Effect::PickFolder(start)]
            }
            SpacesMsg::FolderPicked(picked) => {
                let Some(offer) = &mut self.offer else { return Vec::new() };
                if let Some(folder) = picked.filter(|f| !f.trim().is_empty()) {
                    offer.folder = Some(folder);
                    offer.chosen = true;
                    offer.adopt = false;
                    offer.asks_adopt = false;
                    offer.error = None;
                }
                Vec::new()
            }
            SpacesMsg::OfferSync => self.submit_offer(false),
            SpacesMsg::OfferAdopt => self.submit_offer(true),
            SpacesMsg::OfferSkip => {
                if self.offer.as_ref().is_some_and(|o| !o.busy) {
                    self.offer = None;
                }
                Vec::new()
            }
            SpacesMsg::SyncSet { space, ticket, result } => self.sync_set(space, ticket, result),
            SpacesMsg::OpenFolder(space) => match self.find(space).and_then(|s| s.folder.clone()) {
                Some(folder) => vec![Effect::OpenFolder(folder)],
                None => Vec::new(),
            },
            SpacesMsg::Members { space, ticket, result } => {
                let wanted = self.members_ticket;
                let Some(page) = self.page_mut() else { return Vec::new() };
                if page.space != space || ticket != wanted {
                    return Vec::new();
                }
                page.members.loading = false;
                match result {
                    Ok(list) => {
                        page.members.list = Some(list);
                        page.members.error = None;
                    }
                    Err(e) => {
                        let too_old = is_too_old(&e);
                        page.members.error = Some(words(&e));
                        if too_old {
                            self.too_old();
                        }
                    }
                }
                Vec::new()
            }
            SpacesMsg::StartRename => {
                let Some(space) = self.page().map(|p| p.space) else { return Vec::new() };
                let name = self.find(space).map(space_name).unwrap_or_default();
                if let Some(page) = self.page_mut() {
                    page.rename = Some(name);
                    page.error = None;
                }
                Vec::new()
            }
            SpacesMsg::CancelRename => {
                if self.doing != Some(Doing::Renaming)
                    && let Some(page) = self.page_mut()
                {
                    page.rename = None;
                }
                Vec::new()
            }
            SpacesMsg::SaveRename => self.save_rename(),
            SpacesMsg::Renamed { space, ticket, result } => {
                if !self.answers(ticket) {
                    return Vec::new();
                }
                match result {
                    Ok(name) => {
                        if let Some(info) = self.spaces.iter_mut().flatten().find(|s| s.space == space) {
                            info.name = Some(name);
                        }
                        if let Some(page) = self.page_mut() {
                            page.rename = None;
                            page.error = None;
                        }
                        // The agent keeps the folder's name; the list shows the new one.
                        vec![self.reload()]
                    }
                    Err(e) => self.page_failed(e),
                }
            }
            SpacesMsg::SubmitInvite => self.submit_invite(),
            SpacesMsg::MemberAdded { space, ticket, result } => {
                if !self.answers(ticket) {
                    return Vec::new();
                }
                match result {
                    Ok(member) => {
                        let who = member.username.clone();
                        let Some(page) = self.page_mut() else { return Vec::new() };
                        if page.space != space {
                            return Vec::new();
                        }
                        page.invite = InviteForm { role: page.invite.role, ..InviteForm::default() };
                        page.error = None;
                        self.notice = Some(format!("{who} is invited. They join when they accept."));
                        self.members_again(space)
                    }
                    Err(e) => self.invite_failed(e),
                }
            }
            SpacesMsg::SetRole { user, role } => self.change_role(user, role),
            SpacesMsg::MemberChanged { space, ticket, result } => {
                if !self.answers(ticket) {
                    return Vec::new();
                }
                match result {
                    Ok(_) => {
                        if let Some(page) = self.page_mut() {
                            page.error = None;
                        }
                        self.members_again(space)
                    }
                    Err(e) => {
                        let mut effects = self.page_failed(e);
                        // The server's list is the truth: look again.
                        effects.extend(self.members_again(space));
                        effects
                    }
                }
            }
            SpacesMsg::AskRemove(user) => {
                if self.doing.is_some() {
                    return Vec::new();
                }
                if let Some(page) = self.page_mut() {
                    page.removing = Some(user);
                    page.leaving = false;
                    page.error = None;
                }
                Vec::new()
            }
            SpacesMsg::CancelRemove => {
                if self.doing.is_none()
                    && let Some(page) = self.page_mut()
                {
                    page.removing = None;
                }
                Vec::new()
            }
            SpacesMsg::ConfirmRemove => self.confirm_remove(),
            SpacesMsg::MemberRemoved { space, ticket, result } => {
                if !self.answers(ticket) {
                    return Vec::new();
                }
                if let Some(page) = self.page_mut() {
                    page.removing = None;
                }
                match result {
                    Ok(()) => {
                        self.notice = Some("The member was removed. They keep what they already downloaded.".to_owned());
                        self.members_again(space)
                    }
                    Err(e) => {
                        let mut effects = self.page_failed(e);
                        effects.extend(self.members_again(space));
                        effects
                    }
                }
            }
            SpacesMsg::AskLeave => {
                if self.doing.is_some() {
                    return Vec::new();
                }
                if let Some(page) = self.page_mut() {
                    page.leaving = true;
                    page.removing = None;
                    page.error = None;
                }
                Vec::new()
            }
            SpacesMsg::CancelLeave => {
                if self.doing.is_none()
                    && let Some(page) = self.page_mut()
                {
                    page.leaving = false;
                }
                Vec::new()
            }
            SpacesMsg::ConfirmLeave => self.confirm_leave(),
            SpacesMsg::Left { space, ticket, result } => {
                if !self.answers(ticket) {
                    return Vec::new();
                }
                if let Some(page) = self.page_mut() {
                    page.leaving = false;
                }
                match result {
                    Ok(()) => {
                        let name = self.find(space).map(space_name).unwrap_or_default();
                        self.view = View::List;
                        self.notice = Some(format!("You left {name}. Its folder stays on this computer as it is."));
                        vec![self.reload()]
                    }
                    Err(e) => self.page_failed(e),
                }
            }
            SpacesMsg::CheckDeletes => self.check_deletes(),
            SpacesMsg::ConfirmDeletes => self.confirm_deletes(),
            SpacesMsg::CancelDeletes => {
                if self.doing.is_none()
                    && let Some(page) = self.page_mut()
                {
                    page.deletes = Deletes::Idle;
                }
                Vec::new()
            }
            SpacesMsg::Deletes { space, ticket, result } => self.deletes_came(space, ticket, result),
        }
    }
}

impl SharedScreen {
    /// The effect that asks for the members of the space shown; its answer is the only one wanted.
    fn ask_members(&mut self, space: SpaceId) -> Effect {
        self.members_ticket = fresh();
        let ticket = self.members_ticket;
        if let Some(page) = self.page_mut() {
            page.members.loading = true;
            page.members.error = None;
        }
        Effect::Spaces(SpacesCall::Members { space, ticket })
    }

    /// The members again, after a change, when the page of that space is still shown.
    fn members_again(&mut self, space: SpaceId) -> Vec<Effect> {
        if self.page().is_some_and(|p| p.space == space) { vec![self.ask_members(space)] } else { Vec::new() }
    }

    /// A call of the list failed.
    fn action_failed(&mut self, e: Error) -> Vec<Effect> {
        if is_too_old(&e) {
            self.too_old();
        } else {
            self.error_action = Some(words(&e));
        }
        Vec::new()
    }

    /// A refusal in words of the app's own, on the page of a space when one is shown, else on the list.
    fn page_failed_text(&mut self, text: String) -> Vec<Effect> {
        match self.page_mut() {
            Some(page) => page.error = Some(text),
            None => self.error_action = Some(text),
        }
        Vec::new()
    }

    /// A call of a space's page failed.
    fn page_failed(&mut self, e: Error) -> Vec<Effect> {
        if is_too_old(&e) {
            self.too_old();
            return Vec::new();
        }
        let text = words(&e);
        match self.page_mut() {
            Some(page) => page.error = Some(text),
            None => self.error_action = Some(text),
        }
        Vec::new()
    }

    fn accept(&mut self, space: SpaceId) -> Vec<Effect> {
        if self.doing.is_some() || !self.invitations().iter().any(|s| s.space == space) {
            return Vec::new();
        }
        self.error_action = None;
        self.notice = None;
        self.doing = Some(Doing::Accepting(space));
        let ticket = self.next_ticket();
        vec![Effect::Spaces(SpacesCall::Accept { space, ticket })]
    }

    /// After Accept: the list again (the agent knows the space and its default folder now) and the offer to sync it, which
    /// leaves the space off unless the person says so.
    fn accepted(&mut self, space: SpaceId, ticket: u64, result: Result<(), Error>) -> Vec<Effect> {
        if !self.answers(ticket) {
            return Vec::new();
        }
        match result {
            Ok(()) => {
                let (name, folder) = self.find(space).map(|s| (space_name(s), s.folder.clone())).unwrap_or_default();
                self.offer = Some(SyncOffer {
                    space,
                    name,
                    folder,
                    chosen: false,
                    adopt: false,
                    asks_adopt: false,
                    retry: false,
                    after_accept: true,
                    busy: false,
                    error: None,
                });
                vec![self.reload()]
            }
            Err(e) => self.action_failed(e),
        }
    }

    /// "Sync on this computer" on a space that is off: the offer, with the agent's folder.
    fn start_offer(&mut self, space: SpaceId) -> Vec<Effect> {
        if self.doing.is_some() {
            return Vec::new();
        }
        let Some(info) = self.find(space) else { return Vec::new() };
        if info.state == "invited" || info.sync == SpaceSync::On || matches!(info.run_state, Some(SpaceRun::NoAccess { .. })) {
            return Vec::new();
        }
        let offer = SyncOffer {
            space,
            name: space_name(info),
            folder: info.folder.clone(),
            chosen: false,
            adopt: false,
            asks_adopt: false,
            retry: false,
            after_accept: false,
            busy: false,
            error: None,
        };
        self.error_action = None;
        self.offer = Some(offer);
        Vec::new()
    }

    /// "Sync" in the offer: the agent turns the space on, in the chosen folder if one was chosen.
    fn submit_offer(&mut self, adopt: bool) -> Vec<Effect> {
        if self.doing.is_some() {
            return Vec::new();
        }
        let Some(offer) = &mut self.offer else { return Vec::new() };
        if offer.busy {
            return Vec::new();
        }
        if adopt && offer.asks_adopt {
            offer.adopt = true;
        }
        // A folder with files is used only when the person said so.
        if offer.asks_adopt && !offer.adopt {
            return Vec::new();
        }
        offer.busy = true;
        offer.error = None;
        offer.retry = false;
        self.retry = None;
        let folder = if offer.chosen { offer.folder.clone() } else { None };
        let request = SpaceSyncRequest { on: true, folder, adopt: offer.adopt };
        let space = offer.space;
        self.doing = Some(Doing::Syncing(space));
        let ticket = self.next_ticket();
        vec![Effect::Spaces(SpacesCall::Sync { space, request, ticket })]
    }

    fn sync_set(&mut self, space: SpaceId, ticket: u64, result: Result<SpaceInfo, Error>) -> Vec<Effect> {
        if !self.answers(ticket) {
            return Vec::new();
        }
        match result {
            Ok(info) => {
                let name = space_name(&info);
                let on = info.sync == SpaceSync::On;
                if let Some(slot) = self.spaces.iter_mut().flatten().find(|s| s.space == space) {
                    *slot = info;
                }
                self.offer = None;
                self.error_action = None;
                self.notice = Some(if on {
                    format!("{name} now syncs to this computer.")
                } else {
                    format!("{name} no longer syncs to this computer. Its folder and files stay where they are.")
                });
                vec![self.reload()]
            }
            Err(e) => {
                if is_too_old(&e) {
                    self.too_old();
                    return Vec::new();
                }
                if is_changing(&e) {
                    self.retry = Some(if self.offer.is_some() { SpacesMsg::OfferSync } else { SpacesMsg::SyncOff(space) });
                    if let Some(offer) = &mut self.offer {
                        offer.busy = false;
                        offer.retry = true;
                        offer.error = Some(CHANGING.to_owned());
                        return Vec::new();
                    }
                    return self.page_failed_text(CHANGING.to_owned());
                }
                if let Some(offer) = &mut self.offer {
                    offer.busy = false;
                    if e.code() == Some(codes::FOLDER_HAS_FILES) {
                        offer.asks_adopt = true;
                        offer.error = Some(
                            "That folder already has files in it. Sync it anyway? Files that differ from the space's are kept as conflict copies; nothing is deleted."
                                .to_owned(),
                        );
                    } else {
                        offer.error = Some(words(&e));
                    }
                    return Vec::new();
                }
                self.action_failed(e)
            }
        }
    }

    fn save_rename(&mut self) -> Vec<Effect> {
        if self.doing.is_some() {
            return Vec::new();
        }
        let Some(page) = self.page() else { return Vec::new() };
        let (space, Some(typed)) = (page.space, page.rename.clone()) else { return Vec::new() };
        let current = self.find(space).map(space_name).unwrap_or_default();
        let name = match craft_cloud_proto::SpaceName::new(&typed) {
            Ok(name) => name.as_str().to_owned(),
            Err(e) => {
                let text = format!("That name won't do: {e}.");
                if let Some(page) = self.page_mut() {
                    page.error = Some(text);
                }
                return Vec::new();
            }
        };
        if name == current {
            if let Some(page) = self.page_mut() {
                page.rename = None;
                page.error = None;
            }
            return Vec::new();
        }
        if let Some(page) = self.page_mut() {
            page.error = None;
        }
        self.doing = Some(Doing::Renaming);
        let ticket = self.next_ticket();
        vec![Effect::Spaces(SpacesCall::Rename { space, name, ticket })]
    }

    fn submit_invite(&mut self) -> Vec<Effect> {
        if self.doing.is_some() {
            return Vec::new();
        }
        let Some(page) = self.page() else { return Vec::new() };
        let space = page.space;
        if !self.is_owner(space) {
            return Vec::new();
        }
        let request = match invitation(&page.invite) {
            Ok(request) => request,
            Err(why) => {
                if let Some(page) = self.page_mut() {
                    page.invite.error = Some(why.to_owned());
                }
                return Vec::new();
            }
        };
        if let Some(page) = self.page_mut() {
            page.invite.error = None;
        }
        self.doing = Some(Doing::Inviting);
        let ticket = self.next_ticket();
        vec![Effect::Spaces(SpacesCall::AddMember { space, request, ticket })]
    }

    fn invite_failed(&mut self, e: Error) -> Vec<Effect> {
        if is_too_old(&e) {
            self.too_old();
            return Vec::new();
        }
        let text = words(&e);
        if let Some(page) = self.page_mut() {
            page.invite.error = Some(text);
        }
        Vec::new()
    }

    fn change_role(&mut self, user: Uuid, role: SpaceRole) -> Vec<Effect> {
        if self.doing.is_some() {
            return Vec::new();
        }
        let Some(page) = self.page() else { return Vec::new() };
        let space = page.space;
        let unchanged = page.members.list.iter().flatten().any(|m| m.user == user && m.role == role);
        if !self.is_owner(space) || unchanged {
            return Vec::new();
        }
        if let Some(page) = self.page_mut() {
            page.error = None;
        }
        self.doing = Some(Doing::ChangingRole(user));
        let ticket = self.next_ticket();
        vec![Effect::Spaces(SpacesCall::ChangeMember { space, user, role, ticket })]
    }

    fn confirm_remove(&mut self) -> Vec<Effect> {
        if self.doing.is_some() {
            return Vec::new();
        }
        let Some(page) = self.page() else { return Vec::new() };
        let (space, Some(user)) = (page.space, page.removing) else { return Vec::new() };
        if !self.is_owner(space) {
            return Vec::new();
        }
        self.doing = Some(Doing::Removing(user));
        let ticket = self.next_ticket();
        vec![Effect::Spaces(SpacesCall::RemoveMember { space, user, ticket })]
    }

    fn confirm_leave(&mut self) -> Vec<Effect> {
        if self.doing.is_some() {
            return Vec::new();
        }
        let Some(page) = self.page() else { return Vec::new() };
        if !page.leaving {
            return Vec::new();
        }
        let space = page.space;
        self.doing = Some(Doing::Leaving);
        let ticket = self.next_ticket();
        vec![Effect::Spaces(SpacesCall::Leave { space, ticket })]
    }

    /// "Approve deletes": the agent lists what the mass-delete guard holds back in this space (and approves nothing yet).
    fn check_deletes(&mut self) -> Vec<Effect> {
        if self.doing.is_some() {
            return Vec::new();
        }
        let Some(page) = self.page() else { return Vec::new() };
        let space = page.space;
        let syncing = self.find(space).is_some_and(|s| s.sync == SpaceSync::On && !s.read_only);
        if !syncing {
            return Vec::new();
        }
        if let Some(page) = self.page_mut() {
            page.deletes = Deletes::Idle;
            page.error = None;
        }
        self.doing = Some(Doing::CheckingDeletes);
        let ticket = self.next_ticket();
        vec![Effect::Spaces(SpacesCall::LookAtDeletes { space, ticket })]
    }

    /// Approves what the look listed: the agent approves the set it listed last.
    fn confirm_deletes(&mut self) -> Vec<Effect> {
        if self.doing.is_some() {
            return Vec::new();
        }
        let Some(page) = self.page() else { return Vec::new() };
        let Deletes::Asking { set, .. } = &page.deletes else { return Vec::new() };
        let (space, set) = (page.space, set.clone());
        self.doing = Some(Doing::ApprovingDeletes);
        let ticket = self.next_ticket();
        vec![Effect::Spaces(SpacesCall::ApproveDeletes { space, set, ticket })]
    }

    fn deletes_came(&mut self, space: SpaceId, ticket: u64, result: Result<ApproveDeletesAnswer, Error>) -> Vec<Effect> {
        let approving = self.doing == Some(Doing::ApprovingDeletes);
        if !self.answers(ticket) {
            return Vec::new();
        }
        if self.page().is_none_or(|p| p.space != space) {
            return Vec::new();
        }
        match result {
            Ok(answer) => {
                let held = held_deletions(&answer.message);
                let set = waiting_set(&answer.message);
                let Some(page) = self.page_mut() else { return Vec::new() };
                page.error = None;
                page.deletes = if approving && answer.approved {
                    Deletes::Approved(held.len())
                } else if held.is_empty() {
                    if approving {
                        page.error =
                            Some("The held deletions changed since they were listed, so nothing was approved. Look at them again.".to_owned());
                    }
                    Deletes::NoneHeld
                } else if let Some(set) = set {
                    if approving {
                        // What was listed changed since: nothing was approved, and the new list is asked about again.
                        page.error =
                            Some("The held deletions changed since they were listed, so nothing was approved. Look at them again.".to_owned());
                    }
                    Deletes::Asking { set, lines: held }
                } else {
                    page.error = Some("The agent listed held deletions without naming them as a set, so they can't be approved here. Approve them with `craft-cloud-agent sync --approve-deletes`.".to_owned());
                    Deletes::Idle
                };
                Vec::new()
            }
            Err(e) => {
                if is_too_old(&e) {
                    self.too_old();
                    return Vec::new();
                }
                // A space that doesn't sync here has nothing to approve: the agent says "no such team space syncing".
                let text = words(&e);
                if let Some(page) = self.page_mut() {
                    page.deletes = Deletes::Idle;
                    page.error = Some(text);
                }
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::tests::{refused, run, signed_in};
    use crate::agent::fake::{FakeAgent, team_member, team_space};
    use crate::model::{Tab, tests::status};

    /// An agent signed in as `fabio` that lists `spaces`.
    fn agent_with(spaces: Vec<SpaceInfo>) -> FakeAgent {
        let agent = FakeAgent::new(status(true));
        agent.team.lock().unwrap().spaces = spaces;
        agent
    }

    fn with_sync(mut info: SpaceInfo, run: SpaceRun) -> SpaceInfo {
        info.sync = SpaceSync::On;
        info.run_state = Some(run);
        info
    }

    /// The model on the Shared tab.
    fn on_shared(agent: &FakeAgent) -> Model {
        let mut model = signed_in(agent);
        run(&mut model, agent, vec![Event::Tab(Tab::Shared)]);
        assert!(model.shared_screen().is_some(), "{:?}", model.screen);
        model
    }

    fn shared(model: &Model) -> &SharedScreen {
        model.shared_screen().unwrap_or_else(|| panic!("not on the Shared tab: {:?}", model.screen))
    }

    fn msg(m: SpacesMsg) -> Vec<Event> {
        vec![Event::Spaces(m)]
    }

    fn log(agent: &FakeAgent) -> Vec<String> {
        agent.team.lock().unwrap().log.clone()
    }

    fn calls_of(agent: &FakeAgent, what: &str) -> usize {
        log(agent).iter().filter(|c| c.starts_with(what)).count()
    }

    /// An owner's space page, opened, with its members loaded: studio (owner ana-the-account is "fabio").
    fn on_owned_space() -> (FakeAgent, Model, SpaceId) {
        let studio = with_sync(team_space("Studio", SpaceRole::Owner, "active"), SpaceRun::Idle);
        let id = studio.space;
        let agent = agent_with(vec![studio]);
        agent.team.lock().unwrap().members.push((
            id,
            vec![
                team_member("fabio", SpaceRole::Owner, "active", 1),
                team_member("ben", SpaceRole::Editor, "active", 2),
                team_member("cleo", SpaceRole::Viewer, "invited", 3),
            ],
        ));
        let mut model = on_shared(&agent);
        run(&mut model, &agent, msg(SpacesMsg::OpenSpace(id)));
        (agent, model, id)
    }

    #[test]
    fn the_shared_tab_lists_the_team_spaces_with_role_sync_state_and_usage() {
        let studio = with_sync(team_space("Studio", SpaceRole::Editor, "active"), SpaceRun::Idle);
        let mut archive = team_space("Archive 2025", SpaceRole::Viewer, "active");
        archive.archived = true;
        archive.read_only = true;
        let mut lost = team_space("Lost", SpaceRole::Editor, "active");
        lost.sync = SpaceSync::Unavailable;
        lost.run_state = Some(SpaceRun::NoAccess { message: "You no longer have access to Lost.".into() });
        let brand = team_space("Brand", SpaceRole::Viewer, "invited");
        let personal = SpaceInfo { kind: "personal".into(), name: None, ..team_space("", SpaceRole::Owner, "active") };
        let (studio_id, archive_id) = (studio.space, archive.space);
        let agent = agent_with(vec![personal, studio, archive, lost, brand]);
        agent.team.lock().unwrap().usage.push((
            studio_id,
            craft_cloud_app::SpaceUsage {
                stored_bytes: 3 << 30,
                quota_bytes: Some(10 << 30),
                trash_bytes: None,
                history_bytes: None,
                live_bytes: 0,
                estimates: craft_cloud_app::UsageEstimates { index_complete: true, reason: None },
            },
        ));
        let model = on_shared(&agent);
        let page = shared(&model);
        assert_eq!(model.tab(), Some(Tab::Shared));
        assert!(page.spaces.as_ref().unwrap().iter().all(|s| s.kind == "team"), "the personal space is the overview's");
        assert_eq!(page.joined().len(), 3);
        assert_eq!(page.invitations().iter().map(|s| space_name(s)).collect::<Vec<_>>(), ["Brand"]);
        assert_eq!(page.overview.invitations, 1);
        let tags = |name: &str| page.joined().into_iter().find(|s| space_name(s) == name).map(|s| state_tags(s)).unwrap();
        assert_eq!(tags("Studio"), [("Synced", Look::Good)]);
        assert_eq!(tags("Archive 2025"), [("Archived", Look::Neutral), ("Read only", Look::Warn), ("Not syncing here", Look::Neutral)]);
        assert_eq!(tags("Lost"), [("No access", Look::Bad)]);
        // What a space stores, against its limit; none for the one the account lost.
        assert_eq!(page.usage_of(studio_id).text(), "3 GiB of 10 GiB");
        assert_eq!(page.usage_of(studio_id).room().map(Room::percent), Some(30));
        assert_eq!(page.usage_of(archive_id).text(), "0 B");
        assert_eq!(calls_of(&agent, "usage:"), 2, "the invitation and the lost space are not asked about: {:?}", log(&agent));
        // The tabs: Shared follows Files.
        let tabs = model.tabs();
        let at = tabs.iter().position(|t| *t == Tab::Shared).unwrap();
        assert_eq!(tabs.get(at.wrapping_sub(1)), Some(&Tab::Files));
        assert_eq!(Tab::Shared.label(), "Shared");
    }

    #[test]
    fn a_state_names_how_the_space_stands_here() {
        let tag = |sync, run, role, archived: bool, read_only: bool| {
            let mut info = team_space("S", role, "active");
            info.sync = sync;
            info.run_state = run;
            info.archived = archived;
            info.read_only = read_only;
            state_tags(&info)
        };
        use SpaceRole::{Editor, Viewer};
        assert_eq!(tag(SpaceSync::On, Some(SpaceRun::Syncing), Editor, false, false), [("Syncing", Look::Good)]);
        assert_eq!(tag(SpaceSync::On, Some(SpaceRun::Retrying { message: "x".into() }), Editor, false, false), [("Retrying", Look::Warn)]);
        assert_eq!(tag(SpaceSync::On, Some(SpaceRun::Paused { reason: "x".into() }), Editor, false, false), [("Needs you", Look::Bad)]);
        assert_eq!(tag(SpaceSync::On, Some(SpaceRun::PausedByUser), Editor, false, false), [("Paused", Look::Neutral)]);
        assert_eq!(tag(SpaceSync::Off, None, Editor, false, false), [("Not syncing here", Look::Neutral)]);
        assert_eq!(tag(SpaceSync::On, Some(SpaceRun::Idle), Viewer, false, true), [("Read only", Look::Warn), ("Synced", Look::Good)]);
        assert_eq!(
            tag(SpaceSync::Unavailable, Some(SpaceRun::Unavailable { message: "update".into() }), Editor, false, false),
            [("Unavailable", Look::Warn)]
        );
        assert!(state_tags(&team_space("S", Editor, "invited")).is_empty(), "an invitation has no state yet");
    }

    #[test]
    fn an_invitation_is_accepted_and_syncing_stays_off_until_the_person_chooses() {
        let brand = team_space("Brand", SpaceRole::Editor, "invited");
        let id = brand.space;
        let agent = agent_with(vec![brand]);
        let mut model = on_shared(&agent);
        assert_eq!(shared(&model).invitations().len(), 1);
        assert_eq!(model.overview().unwrap().invitations, 1);
        let effects = model.update(Event::Spaces(SpacesMsg::Accept(id)));
        assert!(matches!(effects.as_slice(), [Effect::Spaces(SpacesCall::Accept { space, .. })] if *space == id), "{effects:?}");
        assert_eq!(shared(&model).doing, Some(Doing::Accepting(id)));
        assert!(model.update(Event::Spaces(SpacesMsg::Decline(id))).is_empty(), "one call at a time");
        // The answer: a member now, with the offer to sync it, and nothing turned on.
        let mut model = on_shared(&agent);
        run(&mut model, &agent, msg(SpacesMsg::Accept(id)));
        let page = shared(&model);
        assert!(page.invitations().is_empty());
        assert_eq!(page.overview.invitations, 0);
        let offer = page.offer.clone().expect("the offer to sync");
        assert_eq!((offer.space, offer.name.as_str(), offer.chosen, offer.after_accept), (id, "Brand", false, true));
        assert_eq!(offer.folder.as_deref(), Some("/home/fabio/Craft Cloud Shared/Brand"), "the agent's default under Craft Cloud Shared");
        assert!(agent.team.lock().unwrap().syncs.is_empty(), "nothing syncs until the person says so");
        assert_eq!(page.find(id).unwrap().sync, SpaceSync::Off);
        // Sync: the agent's own folder (none sent), no adopt.
        run(&mut model, &agent, msg(SpacesMsg::OfferSync));
        let syncs = agent.team.lock().unwrap().syncs.clone();
        assert_eq!(syncs, [(id, SpaceSyncRequest { on: true, folder: None, adopt: false })]);
        let page = shared(&model);
        assert!(page.offer.is_none() && page.doing.is_none());
        assert_eq!(page.find(id).unwrap().sync, SpaceSync::On);
        assert!(page.notice.as_deref().is_some_and(|n| n.contains("Brand") && n.contains("syncs")), "{:?}", page.notice);
    }

    #[test]
    fn a_space_that_is_still_being_changed_is_tried_again_with_the_same_click() {
        let studio = team_space("Studio", SpaceRole::Editor, "active");
        let id = studio.space;
        let agent = agent_with(vec![studio]);
        let changing =
            || Error::Refused { code: "conflict".into(), message: "this space is being changed, try again in a moment".into(), name_taken: None };
        assert!(is_changing(&changing()) && !is_changing(&refused("conflict", "a team space with that name exists")));
        let mut model = on_shared(&agent);
        run(&mut model, &agent, msg(SpacesMsg::OpenSpace(id)));
        // Turning it on: the offer says so, and Sync again (or Try again) sends the same request.
        model.update(Event::Spaces(SpacesMsg::SyncOn(id)));
        agent.team.lock().unwrap().fails.push(changing());
        run(&mut model, &agent, msg(SpacesMsg::OfferSync));
        let page = shared(&model);
        let offer = page.offer.clone().unwrap();
        assert_eq!((offer.error.as_deref(), offer.retry, offer.busy), (Some(CHANGING), true, false));
        assert!(page.retry.is_some() && page.doing.is_none());
        run(&mut model, &agent, msg(SpacesMsg::Retry));
        assert_eq!(shared(&model).find(id).unwrap().sync, SpaceSync::On);
        assert!(shared(&model).retry.is_none() && shared(&model).offer.is_none());
        assert_eq!(agent.team.lock().unwrap().syncs.len(), 2, "the request went out twice");
        // Stopping: the words show on the space's page, and Try again stops it.
        agent.team.lock().unwrap().fails.push(changing());
        run(&mut model, &agent, msg(SpacesMsg::SyncOff(id)));
        assert_eq!(shared(&model).page().unwrap().error.as_deref(), Some(CHANGING));
        assert_eq!(shared(&model).find(id).unwrap().sync, SpaceSync::On, "nothing changed yet");
        run(&mut model, &agent, msg(SpacesMsg::Retry));
        assert_eq!(shared(&model).find(id).unwrap().sync, SpaceSync::Off);
        // Nothing to retry: nothing is sent.
        assert!(model.update(Event::Spaces(SpacesMsg::Retry)).is_empty());
    }

    #[test]
    fn not_now_leaves_the_space_off() {
        let brand = team_space("Brand", SpaceRole::Editor, "invited");
        let id = brand.space;
        let agent = agent_with(vec![brand]);
        let mut model = on_shared(&agent);
        run(&mut model, &agent, msg(SpacesMsg::Accept(id)));
        assert!(shared(&model).offer.is_some());
        run(&mut model, &agent, msg(SpacesMsg::OfferSkip));
        assert!(shared(&model).offer.is_none());
        assert!(agent.team.lock().unwrap().syncs.is_empty());
        assert_eq!(shared(&model).find(id).unwrap().sync, SpaceSync::Off);
        // Later, from the space's page, it can be turned on (and the offer comes again).
        run(&mut model, &agent, msg(SpacesMsg::OpenSpace(id)));
        assert!(model.update(Event::Spaces(SpacesMsg::SyncOn(id))).is_empty());
        assert!(shared(&model).offer.as_ref().is_some_and(|o| !o.after_accept && o.space == id));
    }

    #[test]
    fn another_folder_is_chosen_and_a_folder_with_files_asks_first() {
        let brand = team_space("Brand", SpaceRole::Editor, "invited");
        let id = brand.space;
        let agent = agent_with(vec![brand]);
        let mut model = on_shared(&agent);
        run(&mut model, &agent, msg(SpacesMsg::Accept(id)));
        // The picker starts at the agent's folder; its answer is the folder that is sent.
        let effects = model.update(Event::Spaces(SpacesMsg::OfferChoose));
        assert_eq!(effects, [Effect::PickFolder("/home/fabio/Craft Cloud Shared/Brand".into())]);
        model.update(Event::FolderPicked(None));
        assert!(!shared(&model).offer.as_ref().unwrap().chosen, "a cancelled picker changes nothing");
        model.update(Event::FolderPicked(Some("/data/Brand".into())));
        let offer = shared(&model).offer.clone().unwrap();
        assert_eq!((offer.folder.as_deref(), offer.chosen), (Some("/data/Brand"), true));
        // The folder has files: the agent refuses without adopt, and the offer asks.
        agent.team.lock().unwrap().folder_with_files = Some("/data/Brand".into());
        run(&mut model, &agent, msg(SpacesMsg::OfferSync));
        let offer = shared(&model).offer.clone().unwrap();
        assert!(offer.asks_adopt && !offer.busy && !offer.adopt, "{offer:?}");
        assert!(offer.error.as_deref().is_some_and(|e| e.contains("already has files")), "{:?}", offer.error);
        assert!(shared(&model).doing.is_none());
        assert!(model.update(Event::Spaces(SpacesMsg::OfferSync)).is_empty(), "files in the folder need the person's yes");
        run(&mut model, &agent, msg(SpacesMsg::OfferAdopt));
        let syncs = agent.team.lock().unwrap().syncs.clone();
        assert_eq!(syncs.last(), Some(&(id, SpaceSyncRequest { on: true, folder: Some("/data/Brand".into()), adopt: true })));
        assert!(shared(&model).offer.is_none());
        assert_eq!(shared(&model).find(id).unwrap().folder.as_deref(), Some("/data/Brand"));
    }

    #[test]
    fn declining_removes_the_invitation_and_a_refusal_is_said() {
        let brand = team_space("Brand", SpaceRole::Editor, "invited");
        let id = brand.space;
        let agent = agent_with(vec![brand]);
        let mut model = on_shared(&agent);
        agent.team.lock().unwrap().fails.push(refused("not_found", "server: not found"));
        run(&mut model, &agent, msg(SpacesMsg::Decline(id)));
        let page = shared(&model);
        assert_eq!(page.error_action.as_deref(), Some("server: not found"));
        assert_eq!(page.invitations().len(), 1, "still there");
        assert!(page.doing.is_none());
        run(&mut model, &agent, msg(SpacesMsg::Decline(id)));
        let page = shared(&model);
        assert!(page.invitations().is_empty() && page.error_action.is_none());
        assert_eq!(page.overview.invitations, 0);
        assert!(page.notice.as_deref().is_some_and(|n| n.contains("declined") && n.contains("Brand")));
        // A space that isn't an invitation can't be declined.
        assert!(model.update(Event::Spaces(SpacesMsg::Decline(id))).is_empty());
    }

    #[test]
    fn the_sync_is_turned_on_and_off_on_a_spaces_page_and_the_folder_stays() {
        let studio = team_space("Studio", SpaceRole::Editor, "active");
        let id = studio.space;
        let agent = agent_with(vec![studio]);
        let mut model = on_shared(&agent);
        run(&mut model, &agent, msg(SpacesMsg::OpenSpace(id)));
        assert_eq!(shared(&model).page().map(|p| p.space), Some(id));
        model.update(Event::Spaces(SpacesMsg::SyncOn(id)));
        assert!(shared(&model).offer.is_some());
        run(&mut model, &agent, msg(SpacesMsg::OfferSync));
        assert_eq!(shared(&model).find(id).unwrap().sync, SpaceSync::On);
        assert!(model.update(Event::Spaces(SpacesMsg::SyncOn(id))).is_empty(), "already on");
        run(&mut model, &agent, msg(SpacesMsg::SyncOff(id)));
        let page = shared(&model);
        let info = page.find(id).unwrap();
        assert_eq!((info.sync, info.folder.as_deref()), (SpaceSync::Off, Some("/home/fabio/Craft Cloud Shared/Studio")));
        assert!(page.notice.as_deref().is_some_and(|n| n.contains("no longer syncs") && n.contains("stay")), "{:?}", page.notice);
        let syncs = agent.team.lock().unwrap().syncs.clone();
        assert_eq!(syncs.last().map(|(_, r)| (r.on, r.folder.clone())), Some((false, None)));
        // Open folder asks the file manager for the space's folder.
        assert_eq!(model.update(Event::Spaces(SpacesMsg::OpenFolder(id))), [Effect::OpenFolder("/home/fabio/Craft Cloud Shared/Studio".into())]);
    }

    #[test]
    fn a_viewer_sees_no_owner_controls_and_the_model_refuses_them() {
        let docs = with_sync(team_space("Docs", SpaceRole::Viewer, "active"), SpaceRun::Idle);
        let id = docs.space;
        let agent = agent_with(vec![docs]);
        agent
            .team
            .lock()
            .unwrap()
            .members
            .push((id, vec![team_member("ana", SpaceRole::Owner, "active", 1), team_member("fabio", SpaceRole::Viewer, "active", 2)]));
        let mut model = on_shared(&agent);
        run(&mut model, &agent, msg(SpacesMsg::OpenSpace(id)));
        let page = shared(&model);
        assert!(!page.is_owner(id));
        assert_eq!(page.page().and_then(|p| p.members.list.as_ref()).map(Vec::len), Some(2), "any member reads the list");
        let before = log(&agent).len();
        let ana = Uuid::from_u128(0x7001);
        for event in [
            SpacesMsg::StartRename,
            SpacesMsg::SaveRename,
            SpacesMsg::SubmitInvite,
            SpacesMsg::SetRole { user: ana, role: SpaceRole::Viewer },
            SpacesMsg::AskRemove(ana),
            SpacesMsg::ConfirmRemove,
        ] {
            let effects = model.update(Event::Spaces(event.clone()));
            assert!(effects.is_empty(), "{event:?} -> {effects:?}");
        }
        assert_eq!(log(&agent).len(), before);
        // It can leave, like any role.
        assert!(model.update(Event::Spaces(SpacesMsg::AskLeave)).is_empty());
        assert!(shared(&model).page().unwrap().leaving);
    }

    #[test]
    fn archived_and_lost_spaces_offer_nothing_to_change() {
        let mut archived = with_sync(team_space("Old", SpaceRole::Owner, "active"), SpaceRun::Idle);
        archived.archived = true;
        archived.read_only = true;
        let mut lost = team_space("Gone", SpaceRole::Editor, "active");
        lost.sync = SpaceSync::Unavailable;
        lost.run_state =
            Some(SpaceRun::NoAccess { message: "You no longer have access to Gone. Your local copy is kept and no longer syncs.".into() });
        let (old, gone) = (archived.space, lost.space);
        let agent = agent_with(vec![archived, lost]);
        let mut model = on_shared(&agent);
        // A lost space: nothing to turn on, no deletes to approve, no members to ask about.
        run(&mut model, &agent, msg(SpacesMsg::OpenSpace(gone)));
        assert!(model.update(Event::Spaces(SpacesMsg::SyncOn(gone))).is_empty());
        assert!(model.update(Event::Spaces(SpacesMsg::CheckDeletes)).is_empty());
        assert!(!shared(&model).is_owner(gone));
        assert_eq!(state_tags(shared(&model).find(gone).unwrap()), [("No access", Look::Bad)]);
        // The local copy is kept: its folder is still named and can be opened.
        assert_eq!(model.update(Event::Spaces(SpacesMsg::OpenFolder(gone))), [Effect::OpenFolder("/home/fabio/Craft Cloud Shared/Gone".into())]);
        // An archived space reads only: no rename, and it still syncs read-only.
        run(&mut model, &agent, msg(SpacesMsg::BackToList));
        run(&mut model, &agent, msg(SpacesMsg::OpenSpace(old)));
        assert!(model.update(Event::Spaces(SpacesMsg::CheckDeletes)).is_empty(), "a read-only space has nothing held back");
        assert!(shared(&model).find(old).unwrap().archived);
    }

    #[test]
    fn held_deletes_are_listed_then_approved_after_a_question() {
        let studio = with_sync(team_space("Studio", SpaceRole::Editor, "active"), SpaceRun::Idle);
        let id = studio.space;
        let agent = agent_with(vec![studio]);
        agent.team.lock().unwrap().held = vec!["Shots/a.psd".into(), "Shots/b.psd".into(), "Notes.txt".into()];
        let mut model = on_shared(&agent);
        run(&mut model, &agent, msg(SpacesMsg::OpenSpace(id)));
        // The first look lists what is held back and approves nothing.
        run(&mut model, &agent, msg(SpacesMsg::CheckDeletes));
        let deletes = shared(&model).page().unwrap().deletes.clone();
        let Deletes::Asking { set, lines } = deletes else { panic!("{deletes:?}") };
        assert_eq!((set.as_str(), lines.len()), ("abc123", 3));
        assert!(lines[0].starts_with("delete on the server: Shots/a.psd"));
        assert_eq!(agent.team.lock().unwrap().approved_deletes, 0, "listing approves nothing");
        // The person says no: nothing is approved and the deletions stay held.
        model.update(Event::Spaces(SpacesMsg::CancelDeletes));
        assert_eq!(shared(&model).page().unwrap().deletes, Deletes::Idle);
        assert_eq!(agent.team.lock().unwrap().approved_deletes, 0);
        // Looking again never approves, however many times the question was asked and dropped.
        run(&mut model, &agent, msg(SpacesMsg::CheckDeletes));
        run(&mut model, &agent, msg(SpacesMsg::CancelDeletes));
        run(&mut model, &agent, msg(SpacesMsg::CheckDeletes));
        assert!(matches!(shared(&model).page().unwrap().deletes, Deletes::Asking { .. }));
        assert_eq!(agent.team.lock().unwrap().approved_deletes, 0);
        // Yes: the call that names the listed set approves it.
        let asked = agent.team.lock().unwrap().deletes_sets.clone();
        assert!(asked.iter().all(|s| s.as_deref() == Some("look")), "{asked:?}");
        run(&mut model, &agent, msg(SpacesMsg::ConfirmDeletes));
        assert_eq!(shared(&model).page().unwrap().deletes, Deletes::Approved(3));
        assert_eq!(agent.team.lock().unwrap().approved_deletes, 3);
        assert_eq!(agent.team.lock().unwrap().deletes_sets.last().cloned().flatten().as_deref(), Some("abc123"), "only the set that was listed");
        // Another look finds nothing held.
        run(&mut model, &agent, msg(SpacesMsg::CheckDeletes));
        assert_eq!(shared(&model).page().unwrap().deletes, Deletes::NoneHeld);
        // Nothing is approved without the question first.
        assert!(model.update(Event::Spaces(SpacesMsg::ConfirmDeletes)).is_empty());
    }

    #[test]
    fn deletes_that_changed_after_they_were_listed_are_not_approved() {
        let studio = with_sync(team_space("Studio", SpaceRole::Editor, "active"), SpaceRun::Idle);
        let id = studio.space;
        let agent = agent_with(vec![studio]);
        let mut model = on_shared(&agent);
        run(&mut model, &agent, msg(SpacesMsg::OpenSpace(id)));
        let effects = model.update(Event::Spaces(SpacesMsg::CheckDeletes));
        let Some(Effect::Spaces(SpacesCall::LookAtDeletes { ticket, .. })) = effects.first().cloned() else { panic!("{effects:?}") };
        let listed = ApproveDeletesAnswer { approved: false, message: "waiting for approval, set a1\n  delete on the server: a.txt\n".into() };
        model.update(Event::Spaces(SpacesMsg::Deletes { space: id, ticket, result: Ok(listed) }));
        let effects = model.update(Event::Spaces(SpacesMsg::ConfirmDeletes));
        let Some(Effect::Spaces(SpacesCall::ApproveDeletes { ticket, set, .. })) = effects.first().cloned() else { panic!("{effects:?}") };
        assert_eq!(set, "a1", "the set that was listed, not whatever is waiting later");
        // The agent says the set changed: nothing approved, the new list is asked about again.
        let changed = ApproveDeletesAnswer {
            approved: false,
            message: "waiting for approval, set b2\n  delete on the server: a.txt\n  delete here: b.txt\nnothing approved: these deletions changed"
                .into(),
        };
        model.update(Event::Spaces(SpacesMsg::Deletes { space: id, ticket, result: Ok(changed) }));
        let page = shared(&model).page().unwrap();
        assert!(matches!(&page.deletes, Deletes::Asking { set, lines } if set == "b2" && lines.len() == 2), "{:?}", page.deletes);
        assert!(page.error.as_deref().is_some_and(|e| e.contains("nothing was approved")), "{:?}", page.error);
        // A space the agent doesn't sync answers its own words, which are shown.
        model.update(Event::Spaces(SpacesMsg::CancelDeletes));
        let effects = model.update(Event::Spaces(SpacesMsg::CheckDeletes));
        let Some(Effect::Spaces(SpacesCall::LookAtDeletes { ticket, .. })) = effects.first().cloned() else { panic!("{effects:?}") };
        model.update(Event::Spaces(SpacesMsg::Deletes {
            space: id,
            ticket,
            result: Err(refused("not_found", "no such team space syncing on this computer")),
        }));
        let page = shared(&model).page().unwrap();
        assert_eq!((&page.deletes, page.error.as_deref()), (&Deletes::Idle, Some("no such team space syncing on this computer")));
    }

    #[test]
    fn the_waiting_set_is_read_from_the_listing() {
        assert_eq!(
            waiting_set("waiting for approval, set 3f9a0b1c (check the list, then run this again):\n  delete here: x"),
            Some("3f9a0b1c".into())
        );
        assert_eq!(waiting_set("waiting for approval, set abc123"), Some("abc123".into()));
        assert_eq!(waiting_set("nothing was waiting for approval"), None);
        assert_eq!(waiting_set("waiting for approval, set "), None);
    }

    #[test]
    fn held_deletions_are_read_from_the_agents_lines() {
        let message = "held, not deleted on the server: x\n  held.txt (held while any of l is a link)\nwaiting for approval, set 3f9a (check the list):\n  delete on the server: a/b.txt\n  delete here: c.txt\nnothing approved";
        assert_eq!(held_deletions(message), ["delete on the server: a/b.txt", "delete here: c.txt"]);
        assert!(held_deletions("nothing was waiting for approval").is_empty());
        assert!(held_deletions("").is_empty());
    }

    #[test]
    fn an_owner_sees_the_members_invites_changes_roles_and_removes() {
        let (agent, mut model, id) = on_owned_space();
        let page = shared(&model);
        assert!(page.is_owner(id));
        let members = page.page().unwrap().members.list.clone().unwrap();
        assert_eq!(
            members.iter().map(|m| (m.username.as_str(), m.role, m.state.as_str())).collect::<Vec<_>>(),
            [("fabio", SpaceRole::Owner, "active"), ("ben", SpaceRole::Editor, "active"), ("cleo", SpaceRole::Viewer, "invited"),]
        );
        // The invitation form checks the name before anything is sent.
        let before = log(&agent).len();
        assert!(model.update(Event::Spaces(SpacesMsg::SubmitInvite)).is_empty());
        assert_eq!(shared(&model).page().unwrap().invite.error.as_deref(), Some(INVITE_EMPTY));
        if let Some(page) = shared_mut(&mut model).page_mut() {
            page.invite.name = "not a name!".into();
        }
        assert!(model.update(Event::Spaces(SpacesMsg::SubmitInvite)).is_empty());
        assert_eq!(shared(&model).page().unwrap().invite.error.as_deref(), Some(crate::admin::BAD_USERNAME));
        assert_eq!(log(&agent).len(), before);
        // A good name with a role goes to the server (an owner sends no confirmation).
        if let Some(page) = shared_mut(&mut model).page_mut() {
            page.invite = InviteForm { name: " dana ".into(), role: SpaceRole::Viewer, error: None };
        }
        let effects = model.update(Event::Spaces(SpacesMsg::SubmitInvite));
        assert!(
            matches!(effects.as_slice(), [Effect::Spaces(SpacesCall::AddMember { request, .. })] if request.username == "dana" && request.role == SpaceRole::Viewer && request.confirm == SpaceConfirm::default()),
            "{effects:?}"
        );
        settle(&mut model, &agent, effects);
        let page = shared(&model);
        assert!(page.notice.as_deref().is_some_and(|n| n.contains("dana") && n.contains("accept")), "{:?}", page.notice);
        assert!(page.page().unwrap().members.list.as_ref().unwrap().iter().any(|m| m.username == "dana" && m.state == "invited"));
        assert_eq!(page.page().unwrap().invite.name, "", "the field is emptied; the role stays for the next one");
        assert_eq!(page.page().unwrap().invite.role, SpaceRole::Viewer);
        // A role change goes through at once; the same role sends nothing.
        let ben = Uuid::from_u128(0x7002);
        assert!(model.update(Event::Spaces(SpacesMsg::SetRole { user: ben, role: SpaceRole::Editor })).is_empty(), "unchanged");
        run(&mut model, &agent, msg(SpacesMsg::SetRole { user: ben, role: SpaceRole::Viewer }));
        let members = shared(&model).page().unwrap().members.list.clone().unwrap();
        assert_eq!(members.iter().find(|m| m.user == ben).map(|m| m.role), Some(SpaceRole::Viewer));
        // Remove asks first and says the person keeps what they downloaded; Keep sends nothing.
        model.update(Event::Spaces(SpacesMsg::AskRemove(ben)));
        assert_eq!(shared(&model).page().unwrap().removing, Some(ben));
        let before = log(&agent).len();
        model.update(Event::Spaces(SpacesMsg::CancelRemove));
        assert_eq!((shared(&model).page().unwrap().removing, log(&agent).len()), (None, before));
        model.update(Event::Spaces(SpacesMsg::AskRemove(ben)));
        run(&mut model, &agent, msg(SpacesMsg::ConfirmRemove));
        let page = shared(&model);
        assert!(page.page().unwrap().members.list.as_ref().unwrap().iter().all(|m| m.user != ben));
        assert!(page.notice.as_deref().is_some_and(|n| n.contains("keep what they already downloaded")));
        assert_eq!(page.page().unwrap().removing, None);
    }

    /// Answers `effects` with the fake, and what the model asks next, until nothing is left (as the link does).
    fn settle(model: &mut Model, agent: &FakeAgent, effects: Vec<Effect>) {
        let start: crate::agent::Starter = std::sync::Arc::new(|_: &dyn crate::agent::Agent| Err(String::new()));
        let mut pending = effects;
        while let Some(effect) = pending.pop() {
            if let Some(answer) = crate::agent::call(agent, &start, effect) {
                pending.extend(model.update(answer));
            }
        }
    }

    fn shared_mut(model: &mut Model) -> &mut SharedScreen {
        match &mut model.screen {
            Screen::Shared(shared) => shared,
            other => panic!("not on the Shared tab: {other:?}"),
        }
    }

    #[test]
    fn the_servers_refusals_of_member_changes_are_shown_where_they_happened() {
        let (agent, mut model, id) = on_owned_space();
        let _ = id;
        // An account that doesn't exist (or can't be invited): one answer, on the form.
        if let Some(page) = shared_mut(&mut model).page_mut() {
            page.invite.name = "nobody".into();
        }
        run(&mut model, &agent, msg(SpacesMsg::SubmitInvite));
        let page = shared(&model);
        assert_eq!(page.page().unwrap().invite.error.as_deref(), Some("no such account"));
        assert!(page.doing.is_none(), "the buttons are free again");
        // The last owner can't be demoted: the server's words show, and the list is asked again.
        agent.team.lock().unwrap().fails.push(refused("last_owner", "a space keeps at least one owner"));
        let fabio = Uuid::from_u128(0x7001);
        let asked = calls_of(&agent, "members:");
        run(&mut model, &agent, msg(SpacesMsg::SetRole { user: fabio, role: SpaceRole::Editor }));
        assert_eq!(shared(&model).page().unwrap().error.as_deref(), Some("a space keeps at least one owner"));
        assert_eq!(calls_of(&agent, "members:"), asked + 1, "the server's list is the truth");
        // A removal that fails closes its question.
        agent.team.lock().unwrap().fails.push(refused("read_only", "your role in this space does not allow changes"));
        model.update(Event::Spaces(SpacesMsg::AskRemove(Uuid::from_u128(0x7002))));
        run(&mut model, &agent, msg(SpacesMsg::ConfirmRemove));
        let page = shared(&model).page().unwrap();
        assert_eq!(page.removing, None);
        assert_eq!(page.error.as_deref(), Some("your role in this space does not allow changes"));
    }

    #[test]
    fn leaving_asks_first_and_the_local_copy_stays() {
        let (agent, mut model, id) = on_owned_space();
        let before = log(&agent).len();
        model.update(Event::Spaces(SpacesMsg::AskLeave));
        assert!(shared(&model).page().unwrap().leaving);
        model.update(Event::Spaces(SpacesMsg::CancelLeave));
        assert!(!shared(&model).page().unwrap().leaving);
        assert_eq!(log(&agent).len(), before, "nothing is sent until the person confirms");
        assert!(model.update(Event::Spaces(SpacesMsg::ConfirmLeave)).is_empty(), "no question open: nothing to confirm");
        // The last owner may not leave: the server says so; the page stays.
        agent.team.lock().unwrap().fails.push(refused("last_owner", "a space keeps at least one owner"));
        model.update(Event::Spaces(SpacesMsg::AskLeave));
        run(&mut model, &agent, msg(SpacesMsg::ConfirmLeave));
        let page = shared(&model);
        assert_eq!(page.page().map(|p| (p.leaving, p.error.clone())), Some((false, Some("a space keeps at least one owner".into()))));
        // Another try works: back on the list, which no longer has the space, and a note says the folder stays.
        model.update(Event::Spaces(SpacesMsg::AskLeave));
        run(&mut model, &agent, msg(SpacesMsg::ConfirmLeave));
        let page = shared(&model);
        assert!(page.page().is_none(), "back on the list");
        assert!(page.find(id).is_none());
        assert!(page.notice.as_deref().is_some_and(|n| n.contains("You left Studio") && n.contains("folder stays")), "{:?}", page.notice);
        assert_eq!(calls_of(&agent, "leave:"), 2);
    }

    #[test]
    fn renaming_is_checked_and_the_new_name_shows() {
        let (agent, mut model, id) = on_owned_space();
        model.update(Event::Spaces(SpacesMsg::StartRename));
        assert_eq!(shared(&model).page().unwrap().rename.as_deref(), Some("Studio"));
        let set = |model: &mut Model, text: &str| {
            if let Some(page) = shared_mut(model).page_mut() {
                page.rename = Some(text.to_owned());
            }
        };
        set(&mut model, "   ");
        assert!(model.update(Event::Spaces(SpacesMsg::SaveRename)).is_empty());
        assert!(shared(&model).page().unwrap().error.as_deref().is_some_and(|e| e.starts_with("That name won't do")));
        set(&mut model, &"x".repeat(65));
        assert!(model.update(Event::Spaces(SpacesMsg::SaveRename)).is_empty());
        // The name it has: nothing to send, the field closes.
        set(&mut model, "Studio");
        assert!(model.update(Event::Spaces(SpacesMsg::SaveRename)).is_empty());
        assert_eq!(shared(&model).page().unwrap().rename, None);
        // A new one: sent trimmed; a refusal keeps the field with the reason.
        model.update(Event::Spaces(SpacesMsg::StartRename));
        set(&mut model, "Studio Two");
        agent.team.lock().unwrap().fails.push(refused("conflict", "a team space with that name exists"));
        run(&mut model, &agent, msg(SpacesMsg::SaveRename));
        let page = shared(&model).page().unwrap();
        assert_eq!((page.rename.as_deref(), page.error.as_deref()), (Some("Studio Two"), Some("a team space with that name exists")));
        run(&mut model, &agent, msg(SpacesMsg::SaveRename));
        let page = shared(&model);
        assert_eq!(page.page().unwrap().rename, None);
        assert_eq!(page.find(id).and_then(|s| s.name.as_deref()), Some("Studio Two"));
        let updates = agent.team.lock().unwrap().updates.clone();
        assert_eq!(updates.last().map(|(_, u)| u.name.clone()), Some(Some("Studio Two".to_owned())));
    }

    #[test]
    fn an_agent_that_is_too_old_ends_in_one_message_and_nothing_else_is_asked() {
        let agent = agent_with(vec![team_space("Studio", SpaceRole::Editor, "active")]);
        agent.team.lock().unwrap().too_old = true;
        let mut model = on_shared(&agent);
        let page = shared(&model);
        assert!(page.too_old);
        assert!(page.spaces.is_none() && page.error.is_none() && !page.loading);
        assert_eq!(crate::spaces::TOO_OLD, "Update Craft Cloud on this computer to use team spaces");
        // Whatever is clicked, no other team call goes out.
        let id = SpaceId::generate();
        for event in [SpacesMsg::Accept(id), SpacesMsg::OpenSpace(id), SpacesMsg::SyncOn(id), SpacesMsg::CheckDeletes] {
            assert!(model.update(Event::Spaces(event)).is_empty());
        }
        assert!(calls_of(&agent, "spaces") >= 1);
        assert_eq!(log(&agent).iter().filter(|c| !c.starts_with("spaces")).count(), 0, "{:?}", log(&agent));
        // A call answered as too old later shows the same message, not the agent's own words.
        let agent = agent_with(vec![team_space("Brand", SpaceRole::Editor, "invited")]);
        let brand = agent.team.lock().unwrap().spaces[0].space;
        let mut model = on_shared(&agent);
        agent.team.lock().unwrap().too_old = true;
        run(&mut model, &agent, msg(SpacesMsg::Accept(brand)));
        let page = shared(&model);
        assert!(page.too_old && page.error_action.is_none() && page.doing.is_none() && page.offer.is_none());
        // The words come from the error code, not from the agent's text.
        assert_eq!(words(&refused("agent_too_old", "anything")), TOO_OLD);
        assert_eq!(crate::admin::error_text(&refused("agent_too_old", "anything")), TOO_OLD);
    }

    #[test]
    fn a_list_that_fails_says_so_and_the_next_look_recovers() {
        let agent = agent_with(vec![team_space("Studio", SpaceRole::Editor, "active")]);
        agent.team.lock().unwrap().fails.push(Error::Io("the connection broke".into()));
        let mut model = signed_in(&agent);
        // The first look (the invitations' count) failing says nothing on the page; the tab's own look shows it.
        agent.team.lock().unwrap().fails.push(Error::Io("the connection broke".into()));
        run(&mut model, &agent, vec![Event::Tab(Tab::Shared)]);
        let page = shared(&model);
        assert!(page.spaces.is_none() && !page.loading && !page.too_old);
        assert!(page.error.as_deref().is_some_and(|e| e.contains("connection to the Craft Cloud agent broke")), "{:?}", page.error);
        run(&mut model, &agent, msg(SpacesMsg::Reload));
        let page = shared(&model);
        assert_eq!(page.joined().len(), 1);
        assert!(page.error.is_none());
        // A look is not started twice at once.
        model.update(Event::Spaces(SpacesMsg::Reload));
        assert!(model.update(Event::Spaces(SpacesMsg::Reload)).is_empty());
    }

    #[test]
    fn late_and_stale_answers_are_dropped() {
        let brand = team_space("Brand", SpaceRole::Editor, "invited");
        let id = brand.space;
        let agent = agent_with(vec![brand]);
        let mut model = on_shared(&agent);
        let effects = model.update(Event::Spaces(SpacesMsg::Accept(id)));
        let Some(Effect::Spaces(SpacesCall::Accept { ticket, .. })) = effects.first().cloned() else { panic!("{effects:?}") };
        // An answer for another call does nothing.
        assert!(model.update(Event::Spaces(SpacesMsg::Accepted { space: id, ticket: ticket + 1, result: Ok(()) })).is_empty());
        assert_eq!(shared(&model).doing, Some(Doing::Accepting(id)));
        // An answer for the list asked before the current one is dropped; so is one for a page that is not shown.
        assert!(model.update(Event::Spaces(SpacesMsg::Listed { ticket: 1, result: Ok(SpaceList::default()) })).is_empty());
        assert!(shared(&model).spaces.is_some(), "the old answer didn't empty the list");
        assert!(model.update(Event::Spaces(SpacesMsg::Members { space: id, ticket: 1, result: Ok(Vec::new()) })).is_empty());
        assert!(model.update(Event::Spaces(SpacesMsg::Usage { space: id, ticket: 1, result: Err(Error::Io("x".into())) })).is_empty());
        // The tab left: an answer that comes after goes nowhere (and doesn't bring the tab back).
        model.update(Event::Tab(Tab::Overview));
        assert!(model.shared_screen().is_none());
        assert!(model.update(Event::Spaces(SpacesMsg::Accepted { space: id, ticket, result: Ok(()) })).is_empty());
        assert!(model.shared_screen().is_none());
    }

    #[test]
    fn a_space_that_leaves_the_list_closes_its_page() {
        let (agent, mut model, id) = on_owned_space();
        agent.team.lock().unwrap().spaces.clear();
        run(&mut model, &agent, msg(SpacesMsg::Reload));
        let page = shared(&model);
        assert!(page.page().is_none() && page.find(id).is_none(), "removed from the space while looking at its page: back to the list");
    }

    #[test]
    fn the_invitations_are_counted_without_opening_the_tab_and_the_tab_follows_the_status() {
        let agent = agent_with(vec![team_space("A", SpaceRole::Editor, "invited"), team_space("B", SpaceRole::Viewer, "invited")]);
        let mut model = signed_in(&agent);
        assert_eq!(model.overview().unwrap().invitations, 2, "asked once the agent answered the account");
        // On the tab, a status poll leaves the page where it is; a space the tab didn't have yet doesn't matter.
        run(&mut model, &agent, vec![Event::Tab(Tab::Shared)]);
        model.update(Event::Polled(Ok(status(true))));
        assert_eq!(model.tab(), Some(Tab::Shared));
        // Refresh (Ctrl+R) is the page's Reload; leaving the tab restores the overview with its count.
        assert_eq!(model.update(Event::RefreshPage).len(), 1);
        run(&mut model, &agent, vec![Event::Tab(Tab::Overview)]);
        assert!(matches!(model.screen, Screen::SignedIn(_)));
        assert_eq!(model.overview().unwrap().invitations, 2);
        // An agent that can't tell says nothing: no count, no message.
        let old = agent_with(vec![]);
        old.team.lock().unwrap().too_old = true;
        let model = signed_in(&old);
        assert_eq!(model.overview().unwrap().invitations, 0);
    }

    #[test]
    fn the_shared_tab_is_left_by_the_other_tabs_and_asks_for_the_others_pages() {
        let agent = agent_with(vec![]);
        let mut model = on_shared(&agent);
        run(&mut model, &agent, vec![Event::Tab(Tab::Files)]);
        assert_eq!(model.tab(), Some(Tab::Files));
        run(&mut model, &agent, vec![Event::Tab(Tab::Shared)]);
        assert_eq!(model.tab(), Some(Tab::Shared));
        run(&mut model, &agent, vec![Event::Tab(Tab::Trash)]);
        assert_eq!(model.tab(), Some(Tab::Trash));
        // The first start's code: Ctrl+3 is the third visible tab (Shared follows Files).
        run(&mut model, &agent, vec![Event::Tab(Tab::Overview)]);
        run(&mut model, &agent, vec![Event::TabNumber(3)]);
        assert_eq!(model.tab(), Some(Tab::Shared));
    }
}
