//! The app's state, as a pure function of what happened. [`Model::update`] takes an [`Event`] (the agent's answer to a
//! call, a click, a tick of the 2 s clock, a second start, the window's close button) and returns the [`Effect`]s to
//! carry out: calls of the agent, opening the folder, quitting the app, the app's own start-at-login entry. Nothing
//! here does I/O or reads a clock, so every screen and every way between them is a unit test.
//!
//! The window is a view of [`Model::screen`]; text fields edit its forms in place, buttons send events.

use std::path::{Path, PathBuf};

use craft_cloud_app::{
    AccountUsage, AuditEvent, AuditPage, DeviceId, DeviceInfo, Error, FolderUsageResponse, HistoryResponse, JoinRequest, LoginRequest, NodeId,
    PairingInfo, PairingState, PairingStatus, RestoreResponse, RunState, SecondFactorLoginRequest, Status, TrashResponse, TrashRestoreResponse,
    UserQuery, Uuid, VersionId, codes,
};
use craft_cloud_proto::api::AdminConfirm;
use craft_cloud_proto::api::codes as api_codes;
use craft_cloud_proto::join::JoinString;

use crate::admin::{Access, Action, AdminEvent, AdminScreen};
use crate::audit::{ActivityScreen, AuditMsg};
use crate::files::{ConflictList, Files, FilesView, Place, RecentFile};
use crate::names::Names;
use crate::security::{SecondFactorForm, SecurityAnswer, SecurityCall, SecurityEvent, SecurityPage, Step as SecurityStep};
use crate::spaces::{SharedScreen, SpacesCall, SpacesMsg};

/// A code that is wrong, expired or used (the server never says which).
pub const INVALID_CODE: &str = "This code doesn't work any more. Make a new one on a signed-in computer.";
/// The agent can't be started.
pub const DIDNT_START: &str = "Craft Cloud's background service didn't start";
/// The agent went away while the window was open.
pub const STOPPED: &str = "Craft Cloud's background service stopped";
/// This device was removed from the account on another computer (or by an admin).
pub const REMOVED: &str = "This computer was removed from your cloud — Join again";
/// Start at login couldn't be turned on after a join (the join stands).
pub const NO_SYNC_AFTER_RESTART: &str = "This computer won't sync after a restart: starting at login couldn't be turned on";
/// A sign-in needs its fields.
pub const SERVER_EMPTY: &str = "Type the server's address";
pub const USER_EMPTY: &str = "Type your user name";
pub const SIGN_IN_PASSWORD_EMPTY: &str = "Type your password";
/// The password of a sign-in is not kept: after "this folder has files in it" it is typed again.
pub const PASSWORD_AGAIN: &str = "Type your password again to sync the folder's files";
/// The server's answer to a password carried no challenge.
pub const NO_CHALLENGE: &str = "The server didn't ask for a code the way it should. Try signing in again.";
/// The challenge of a sign-in ran out.
pub const CHALLENGE_EXPIRED: &str = "This sign-in can't go on any more (it took too long or had too many wrong codes). Sign in again.";
/// The two new passwords differ.
pub const PASSWORDS_DIFFER: &str = "The two passwords aren't the same";
/// No new password was typed.
pub const PASSWORD_EMPTY: &str = "Choose a password for your new account";
/// The folder field holds no full path.
pub const FOLDER_NOT_ABSOLUTE: &str = "Choose the folder to sync by its full path (or with Change…)";

/// What the join form starts with: "Craft Cloud" in the home folder and this computer's name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoinDefaults {
    pub folder: String,
    pub device_name: String,
}

/// What the window shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Screen {
    /// Looking for the agent, or starting it and waiting (up to 5 s) for it to answer.
    Starting,
    /// The agent didn't start, or stopped: the problem and Try again.
    NotRunning(Problem),
    /// Not signed in: "Join your Craft Cloud".
    Join(JoinForm),
    /// The join is with the agent (and the server): nothing can be sent twice.
    Joining { form: JoinForm, password: Option<PasswordForm> },
    /// An invite to a new account: its password, twice.
    PasswordNeeded(PasswordForm),
    /// The account has two-factor sign-in: the join asks for the code ([`SecondFactorForm`]).
    SecondFactor(SecondFactorForm),
    /// Signed in: the account, the sync, the computers.
    SignedIn(Overview),
    /// The Security page ([`SecurityPage`]): two-factor sign-in, the password, the other computers. Boxed: it is large.
    Security(Box<SecurityPage>),
    /// File version history for a path inside the sync root.
    Versions(Versions),
    /// Deleted files and folders awaiting retention expiry.
    Trash(Trash),
    /// The recent files of every Craft app, on every computer of the account.
    Files(Files),
    /// The Shared tab: the team spaces of the account ([`crate::spaces`]). Boxed: it is large.
    Shared(Box<SharedScreen>),
    /// Showing an add-a-computer code.
    AddingComputer(Adding),
    /// The admin pages (an admin's only): the users, one user, a new account, the audit log.
    Admin(Box<AdminScreen>),
    /// The signed-in account's own activity: its security events ([`crate::audit`]).
    Activity(Box<ActivityScreen>),
    /// The server refused this computer: it was removed from the account.
    Removed,
    /// Something the agent can't put right by itself (a damaged account file), or that this app can't work with.
    Error(Problem),
}

/// A problem, as one line plus the whole text under Details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub line: String,
    pub details: String,
    /// What the problem's button does.
    pub action: ProblemAction,
}

/// The button under a [`Problem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProblemAction {
    /// Start the agent again ([`Event::TryAgain`]).
    TryAgain,
    /// Forget the account here and join again ([`Event::ForgetAccount`]).
    ForgetAccount,
    /// Nothing this app can do (another version of the agent): Details only.
    None,
}

/// How the first-start screen joins: with the account's name and password, or with a join string.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JoinMode {
    /// Server, user name and password (and the second factor when the account has one).
    #[default]
    SignIn,
    /// An invite, or the code of a computer that is signed in.
    JoinString,
}

/// The page of the first start's form: one of the three steps of the rail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum JoinStep {
    /// 01: the server's address (a sign-in; a join string names its server itself).
    #[default]
    Server,
    /// 02: user name and password, or the join string.
    Account,
    /// 03: the folder and this computer's name; its Continue signs in or joins.
    Folder,
}

impl JoinStep {
    /// Where the step stands in the rail (0 to 2).
    pub fn index(self) -> usize {
        match self {
            Self::Server => 0,
            Self::Account => 1,
            Self::Folder => 2,
        }
    }

    fn next(self) -> Option<Self> {
        match self {
            Self::Server => Some(Self::Account),
            Self::Account => Some(Self::Folder),
            Self::Folder => None,
        }
    }

    fn prev(self) -> Option<Self> {
        match self {
            Self::Server => None,
            Self::Account => Some(Self::Server),
            Self::Folder => Some(Self::Account),
        }
    }
}

/// The join form, as typed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct JoinForm {
    /// The page being shown. The sign-in or join is made from the last one; a problem with an earlier step's field puts
    /// the form back on that step.
    pub step: JoinStep,
    pub mode: JoinMode,
    /// The join string (or the link) as typed or pasted.
    pub join: String,
    /// Sign-in: the server's address as typed (`https://` is assumed when it has no scheme).
    pub server: String,
    /// Sign-in: the account's user name.
    pub user: String,
    /// Sign-in: the account's password. Only held while it is typed: it is taken out of the form when the request is
    /// made, and cleared when the mode changes.
    pub password: String,
    /// Sign-in: the server's challenge between the password and the second factor, kept while the code is asked for.
    pub challenge: Option<String>,
    /// When the challenge ends (milliseconds since the Unix epoch, as the server said), while there is one.
    pub challenge_expires_ms: i64,
    pub folder: String,
    pub device_name: String,
    /// What went wrong with the last try, for people.
    pub error: Option<String>,
    /// The folder has files in it: the form asks whether to sync them too.
    pub ask_adopt: bool,
    /// The folder whose files the user agreed to sync (`adopt` is sent for it only, so a folder changed after the
    /// question is asked about again).
    pub adopt_for: Option<String>,
}

impl std::fmt::Debug for JoinForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinForm")
            .field("step", &self.step)
            .field("mode", &self.mode)
            .field("join", &"<redacted>")
            .field("server", &self.server)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("challenge", &self.challenge.as_ref().map(|_| "<redacted>"))
            .field("challenge_expires_ms", &self.challenge_expires_ms)
            .field("folder", &self.folder)
            .field("device_name", &self.device_name)
            .field("error", &self.error)
            .field("ask_adopt", &self.ask_adopt)
            .field("adopt_for", &self.adopt_for)
            .finish()
    }
}

/// Whether `address` (`host[:port][/path]`, no scheme) names this computer.
fn is_loopback_host(address: &str) -> bool {
    let authority = address.split('/').next().unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or_default();
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default(),
        None => authority.split(':').next().unwrap_or_default(),
    };
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        || host.parse::<std::net::Ipv6Addr>().is_ok_and(|ip| ip.is_loopback())
}

/// Whether a challenge that ends at `expires_ms` lasts longer than a wait of `retry_after_secs` (unknown: yes).
fn wait_fits(retry_after_secs: Option<u64>, expires_ms: i64, now_ms: i64) -> bool {
    let left_ms = u64::try_from(expires_ms.saturating_sub(now_ms)).unwrap_or(0);
    retry_after_secs.is_none_or(|secs| secs.saturating_mul(1000) < left_ms)
}

/// Where a step of the first start's rail stands: the steps are strictly in sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RailState {
    /// Passed.
    Done,
    /// The one being asked for.
    Current,
    /// Still to come.
    Later,
}

/// One step of the first start's rail: what is known of it so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailStep {
    pub title: &'static str,
    /// What is known (or what the step is for, while it is not).
    pub detail: String,
    pub state: RailState,
}

impl JoinForm {
    fn new(defaults: &JoinDefaults) -> Self {
        Self { folder: defaults.folder.clone(), device_name: defaults.device_name.clone(), ..Self::default() }
    }

    /// The request for the agent: the join string as read (a link, spaces and lower case put right), the folder, the
    /// name (the agent's default when empty), and `adopt` if the user agreed to it for this folder.
    fn request(&self, password: Option<String>) -> JoinRequest {
        let (folder, device_name, adopt) = self.destination();
        JoinRequest { join: self.join.clone(), adopt, folder: Some(folder), device_name, password, totp_code: None, recovery_code: None }
    }

    /// [`JoinForm::request`] with the second factor of an account that has two-factor sign-in.
    fn request_with_factor(&self, password: Option<String>, factor: (Option<String>, Option<String>)) -> JoinRequest {
        JoinRequest { totp_code: factor.0, recovery_code: factor.1, ..self.request(password) }
    }

    /// The folder, the computer's name (`None`: the agent's default) and whether the user agreed to sync the folder's files.
    fn destination(&self) -> (String, Option<String>, bool) {
        let folder = self.folder.trim().to_owned();
        let name = self.device_name.trim();
        let adopt = self.adopt_for.as_deref() == Some(folder.as_str());
        (folder, (!name.is_empty()).then(|| name.to_owned()), adopt)
    }

    /// The server's address for the agent: as typed, with a scheme when it has none: `http://` for a server on this computer
    /// (`localhost`, `127.x.x.x`, `[::1]`: the only ones the agent takes without TLS), `https://` for every other.
    fn server_address(&self) -> String {
        let typed = self.server.trim();
        if typed.contains("://") {
            typed.to_owned()
        } else if is_loopback_host(typed) {
            format!("http://{typed}")
        } else {
            format!("https://{typed}")
        }
    }

    /// The first step of a sign-in; `password` is the one typed (taken out of the form by the caller).
    fn login_request(&self, password: String) -> LoginRequest {
        let (folder, device_name, adopt) = self.destination();
        LoginRequest { server: self.server_address(), user: self.user.trim().to_owned(), password, folder: Some(folder), device_name, adopt }
    }

    /// The second step of a sign-in, with the code; `None` without a challenge.
    fn second_step_request(&self, factor: (Option<String>, Option<String>)) -> Option<SecondFactorLoginRequest> {
        let (folder, device_name, adopt) = self.destination();
        Some(SecondFactorLoginRequest {
            server: self.server_address(),
            user: self.user.trim().to_owned(),
            folder: Some(folder),
            device_name,
            adopt,
            challenge: self.challenge.clone()?,
            totp_code: factor.0,
            recovery_code: factor.1,
        })
    }

    /// The rail with the form's own step current.
    pub fn rail(&self) -> [RailStep; 3] {
        self.rail_at(self.step.index())
    }

    /// The three steps of the first start with what is known of each; the steps before `current` are done, the ones after it
    /// are to come, whatever was typed. No probe of the server is made, so the server is shown as typed (or as the join
    /// string names it), never as "reachable".
    pub fn rail_at(&self, current: usize) -> [RailStep; 3] {
        let state = |n: usize| match n.cmp(&current) {
            std::cmp::Ordering::Less => RailState::Done,
            std::cmp::Ordering::Equal => RailState::Current,
            std::cmp::Ordering::Greater => RailState::Later,
        };
        let parsed = JoinString::parse(&self.join).ok();
        let (server, account) = match self.mode {
            JoinMode::SignIn => {
                let server = self.server.trim();
                let user = self.user.trim();
                ((!server.is_empty()).then(|| crate::ui::host(server).to_owned()), (!user.is_empty()).then(|| user.to_owned()))
            }
            JoinMode::JoinString => {
                (parsed.as_ref().map(|p| crate::ui::host(&p.server).to_owned()), parsed.as_ref().map(|_| "Join string".to_owned()))
            }
        };
        let folder = self.folder.trim();
        [
            RailStep { title: "Server", state: state(0), detail: server.unwrap_or_else(|| "Where your Craft Cloud lives".into()) },
            RailStep { title: "Account", state: state(1), detail: account.unwrap_or_else(|| "Sign in, or use a join string".into()) },
            RailStep {
                title: "Folder and name",
                state: state(2),
                detail: if folder.is_empty() { "Where your files live on this computer".into() } else { folder.to_owned() },
            },
        ]
    }
}

/// The new account's password, typed twice.
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordForm {
    pub form: JoinForm,
    pub password: String,
    pub again: String,
    pub error: Option<String>,
}

impl std::fmt::Debug for PasswordForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PasswordForm").field("form", &self.form).field("error", &self.error).finish_non_exhaustive()
    }
}

/// The least, the most and the usual interface scale, in tenths: 80 % to 160 %, in steps of 10 %.
pub const ZOOM_MIN: u8 = 8;
pub const ZOOM_MAX: u8 = 16;
pub const ZOOM_DEFAULT: u8 = 10;

/// A step of the interface scale (Ctrl+Plus, Ctrl+Minus, Ctrl+0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoomStep {
    In,
    Out,
    Reset,
}

/// The pages of the signed-in window, as the header's tabs list them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    /// File version history ([`Screen::Versions`]).
    Versions,
    /// The recent files of every app ([`Screen::Files`]).
    Files,
    /// The team spaces of the account ([`Screen::Shared`]).
    Shared,
    Trash,
    /// The account's computers, in full ([`OverviewPage::Computers`]).
    Computers,
    Security,
    /// The account's own activity ([`Screen::Activity`]).
    Activity,
    /// The admin pages; offered to an admin only.
    Admin,
}

impl Tab {
    /// The tab's name in the header.
    pub fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Versions => "Versions",
            Tab::Files => "Files",
            Tab::Shared => "Shared",
            Tab::Trash => "Trash",
            Tab::Computers => "Computers",
            Tab::Security => "Security",
            Tab::Activity => "Activity",
            Tab::Admin => "Admin",
        }
    }
}

/// What [`Screen::SignedIn`] shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverviewPage {
    /// The sync, the storage and a short list of the computers.
    #[default]
    Home,
    /// All the computers, with Remove and Add a computer.
    Computers,
}

/// The longest name a computer can have, in bytes (the server's limit).
pub const MAX_DEVICE_NAME_BYTES: usize = 128;

/// A computer's new name being typed on the Computers page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    pub id: DeviceId,
    /// Which try this is: an answer closes the field only when it is for the latest one sent.
    pub ticket: u64,
    /// What the text field holds.
    pub name: String,
    /// The name was sent and the answer is awaited.
    pub busy: bool,
    /// What was wrong with the last try, for people.
    pub error: Option<String>,
}

/// The name to send for a computer, from what was typed: trimmed, 1 to [`MAX_DEVICE_NAME_BYTES`] bytes, no control characters
/// (what the server checks too). The error says what to change.
pub fn check_device_name(typed: &str) -> Result<String, &'static str> {
    let name = typed.trim();
    if name.is_empty() {
        Err("Type a name for this computer.")
    } else if name.len() > MAX_DEVICE_NAME_BYTES {
        Err("That name is too long. Use at most 128 bytes.")
    } else if name.chars().any(char::is_control) {
        Err("A name can't hold control characters.")
    } else {
        Ok(name.to_owned())
    }
}

/// Whether the client version `theirs` is older than `own`. Only dotted numbers are compared (`0.1.0`, an optional `-pre` or
/// `+build` suffix ignored); a version that isn't, or that is equal, is not "older", so the app never claims what it can't know.
pub fn is_older_version(theirs: &str, own: &str) -> bool {
    fn parse(version: &str) -> Option<Vec<u64>> {
        let numbers = version.trim().split(['-', '+']).next()?;
        let parts: Vec<u64> = numbers.split('.').map(|part| part.parse().ok()).collect::<Option<_>>()?;
        (!parts.is_empty() && parts.len() <= 4).then_some(parts)
    }
    let (Some(mut theirs), Some(mut own)) = (parse(theirs), parse(own)) else { return false };
    let len = theirs.len().max(own.len());
    theirs.resize(len, 0);
    own.resize(len, 0);
    theirs < own
}

/// The signed-in view.
#[derive(Debug, Clone, PartialEq)]
pub struct Overview {
    /// Which page of the signed-in view is shown.
    pub page: OverviewPage,
    pub status: Status,
    /// The account's computers, once asked.
    pub devices: Option<Vec<DeviceInfo>>,
    pub devices_error: Option<String>,
    pub usage: Option<AccountUsage>,
    pub usage_loading: bool,
    pub usage_error: Option<String>,
    /// The computer being removed.
    pub removing: Option<DeviceId>,
    /// The computer being renamed on the Computers page: its row is a text field.
    pub renaming: Option<Rename>,
    /// What went wrong with the last action (pause), for people.
    pub error: Option<String>,
    /// Whether the server lets this account use the admin pages (asked at sign-in).
    pub access: Access,
    /// Which sign-in this view is of: answers to the admin check made for another one are ignored.
    pub session: u64,
    /// The newest events of the account's activity, for the home page.
    pub recent: Recent,
    /// The filters of that table (they narrow the events that are loaded).
    pub recent_filter: crate::filter::ActivityFilter,
    /// How many conflict copies the synced folder has, once the agent said (for the home page's tag).
    pub conflicts: Option<ConflictSummary>,
    /// Which answer to [`Effect::Conflicts`] is the current one.
    pub(crate) conflicts_ticket: u64,
    /// The sizes of the synced folder here, by folder and by kind.
    pub folders: Folders,
    /// How many invitations to team spaces wait for an answer (for the tag in the header and on the home page).
    pub invitations: usize,
}

/// The number of conflict copies the agent listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictSummary {
    pub count: usize,
    /// There are more than the agent lists.
    pub truncated: bool,
}

/// The home page's "On this computer": the local logical sizes of the synced folder, asked for when the section is opened.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Folders {
    /// Whether the section is shown.
    pub open: bool,
    pub data: Option<FolderUsageResponse>,
    pub loading: bool,
    /// Why the sizes couldn't be had.
    pub error: Option<String>,
    /// Which answer is the current one.
    pub ticket: u64,
}

/// The home page's short list of the account's newest activity.
#[derive(Debug, Clone, PartialEq)]
pub enum Recent {
    /// Asked for, not answered yet.
    Loading,
    /// The newest events, newest first (at most [`RECENT_ACTIVITY`]).
    Loaded(Vec<AuditEvent>),
    /// The server couldn't tell.
    Failed(String),
}

/// How many events the home page's recent activity shows.
pub const RECENT_ACTIVITY: u32 = 5;

/// File history and restore state.
#[derive(Debug, Clone, PartialEq)]
pub struct Versions {
    pub path: String,
    pub page: Option<HistoryResponse>,
    pub current_version: Option<VersionId>,
    pub loading: bool,
    pub restoring: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub confirm: Option<VersionId>,
    pub ticket: u64,
}

/// Trash list and action state.
#[derive(Debug, Clone, PartialEq)]
pub struct Trash {
    pub page: Option<TrashResponse>,
    pub loading: bool,
    pub action: Option<TrashAction>,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub confirm: Option<NodeId>,
    pub ticket: u64,
    /// Which kind of item the list shows (a filter of the loaded items; nothing is asked of the server).
    pub kind: TrashKind,
    /// What the name or the path of a shown item contains (any case).
    pub search: String,
}

/// The trash list's kind filter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TrashKind {
    #[default]
    All,
    Folders,
    Files,
}

impl Trash {
    /// A trash list, nothing loaded.
    pub fn new() -> Self {
        Self {
            page: None,
            loading: false,
            action: None,
            error: None,
            notice: None,
            confirm: None,
            ticket: 0,
            kind: TrashKind::All,
            search: String::new(),
        }
    }

    /// The loaded items that pass the kind filter and the search.
    pub fn visible(&self) -> Vec<&craft_cloud_app::TrashItem> {
        let needle = self.search.trim().to_lowercase();
        self.page
            .iter()
            .flat_map(|p| p.items.iter())
            .filter(|i| match self.kind {
                TrashKind::All => true,
                TrashKind::Folders => i.is_dir,
                TrashKind::Files => !i.is_dir,
            })
            .filter(|i| {
                needle.is_empty() || i.name.to_lowercase().contains(&needle) || i.path.as_deref().is_some_and(|p| p.to_lowercase().contains(&needle))
            })
            .collect()
    }

    /// How many loaded items there are: all, folders, files.
    pub fn counts(&self) -> (usize, usize, usize) {
        let items = self.page.as_ref().map_or(&[][..], |p| p.items.as_slice());
        let folders = items.iter().filter(|i| i.is_dir).count();
        (items.len(), folders, items.len() - folders)
    }
}

impl Default for Trash {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TrashAction {
    /// `parent_path` is `None` without a path hint and empty for an item whose hint is a file in the sync root.
    Restore {
        path: Option<String>,
        parent_path: Option<String>,
    },
    Remove,
}

impl Overview {
    fn new(status: Status) -> Self {
        Self {
            page: OverviewPage::Home,
            status,
            devices: None,
            devices_error: None,
            usage: None,
            usage_loading: true,
            usage_error: None,
            removing: None,
            renaming: None,
            error: None,
            access: Access::Unknown,
            session: crate::admin::fresh(),
            recent: Recent::Loading,
            recent_filter: crate::filter::ActivityFilter::default(),
            conflicts: None,
            conflicts_ticket: crate::admin::fresh(),
            folders: Folders::default(),
            invitations: 0,
        }
    }

    /// This overview as it is shown again when the screen over it (Security, adding a computer, the admin pages, the activity)
    /// is left. A request for the folder sizes that was under way is not waited for any more: the spinner goes, and an answer
    /// that still comes is taken as it is when it matches.
    pub(crate) fn restored(&self) -> Self {
        let mut overview = self.clone();
        overview.folders.loading = false;
        overview
    }

    /// The effect that asks for the conflict copies again, and makes its answer the current one.
    fn ask_conflicts(&mut self) -> Effect {
        self.conflicts_ticket = crate::admin::fresh();
        Effect::Conflicts { root: self.status.folder.clone(), ticket: self.conflicts_ticket }
    }

    /// The effect that asks for the folder sizes, once at a time.
    fn ask_folders(&mut self) -> Option<Effect> {
        if self.folders.loading {
            return None;
        }
        self.folders.ticket = crate::admin::fresh();
        self.folders.loading = true;
        self.folders.error = None;
        Some(Effect::FolderUsage { ticket: self.folders.ticket })
    }
}

/// Adding a computer: the code, and what came of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Adding {
    pub overview: Overview,
    pub info: Option<PairingInfo>,
    pub state: AddState,
}

/// Where an add-a-computer code stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddState {
    /// Asking the server for a code.
    Asking,
    /// Shown, waiting for the other computer.
    Waiting,
    /// Used: the new computer's name.
    Joined(String),
    /// Ten minutes went by.
    Expired,
    /// No code could be made (the message).
    Failed(String),
}

/// A call that may only run once at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    SigningOut,
    Pausing,
    Quitting,
}

/// What happened.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The app started.
    Start,
    /// The 2 s clock, with the time (milliseconds since the Unix epoch).
    Tick {
        now_ms: i64,
    },
    /// [`Effect::Connect`]'s answer.
    Found(Result<Status, Error>),
    /// [`Effect::StartAgent`]'s answer: the agent's status once it answered, or why it didn't within 5 s.
    AgentStarted(Result<Status, String>),
    /// [`Effect::Status`]'s answer.
    Polled(Result<Status, Error>),
    Joined(Result<Status, Error>),
    /// [`Effect::Login`]'s and [`Effect::LoginSecondFactor`]'s answer.
    LoggedIn(Result<Status, Error>),
    AutostartSet(Result<Status, Error>),
    PairingMade(Result<PairingInfo, Error>),
    PairingPolled(Result<PairingStatus, Error>),
    Devices(Result<Vec<DeviceInfo>, Error>),
    History {
        ticket: u64,
        result: Result<HistoryResponse, Error>,
    },
    Restored {
        ticket: u64,
        result: Result<RestoreResponse, Error>,
    },
    TrashPage {
        ticket: u64,
        result: Result<TrashResponse, Error>,
    },
    TrashRestored {
        ticket: u64,
        result: Result<TrashRestoreResponse, Error>,
    },
    TrashRemoved {
        ticket: u64,
        result: Result<(), Error>,
    },
    Usage {
        request_id: u64,
        result: Result<AccountUsage, Error>,
    },
    /// [`Effect::Recent`]'s answer: the newest events of the account's activity.
    Recent {
        session: u64,
        result: Result<AuditPage, Error>,
    },
    SignedOut(Result<Status, Error>),
    /// [`Effect::Pause`]'s or [`Effect::Resume`]'s answer.
    Answered(Result<Status, Error>),
    /// [`Effect::QuitAgent`] is done (whatever came of it).
    AgentQuit,
    /// [`Effect::AppAutostart`]'s outcome.
    AppAutostartSet(Result<(), String>),

    /// The Security button of the signed-in view.
    OpenSecurity,
    /// A button of the Security page.
    Security(SecurityEvent),
    /// An answer to one of the page's calls.
    SecurityAnswer(SecurityAnswer),
    /// The code of a join that asked for one (the button, or Enter).
    SubmitSecondFactor,
    /// Back from the code field to the join form.
    CancelSecondFactor,
    /// Join or sign in (the form's button, or Enter).
    SubmitJoin,
    /// Continue (the button, or Enter) on a step of the first start: checks the step's fields and moves on; on the last step
    /// it signs in or joins like [`Event::SubmitJoin`].
    JoinNext,
    /// Back (the button, or Esc) to the step before; nothing typed is lost.
    JoinBack,
    /// The form's switch between Sign in and Join string.
    SetJoinMode(JoinMode),
    /// The answer to "this folder has files in it: sync them too?".
    SyncExistingFiles(bool),
    SubmitPassword,
    CancelPassword,
    OpenFolder,
    OpenVersions,
    OpenTrash,
    /// The Files tab: asks for the recent files of every app.
    OpenFiles,
    /// The Files page's Refresh.
    LoadFiles,
    /// The Files page's app filter (`None`: all apps).
    FilesApp(Option<String>),
    /// The Files page's search field, as typed.
    FilesSearch(String),
    /// [`Effect::Recents`]'s answer.
    Recents {
        ticket: u64,
        result: Result<Vec<RecentFile>, Error>,
    },
    /// [`Effect::Conflicts`]'s answer, for the Files page or the home page's tag.
    Conflicts {
        ticket: u64,
        result: Result<ConflictList, Error>,
    },
    /// [`Effect::FolderUsage`]'s answer.
    FolderUsage {
        ticket: u64,
        result: Result<FolderUsageResponse, Error>,
    },
    /// The Files page's switch between the recent files and the conflict copies.
    FilesView(FilesView),
    /// The home page's "n conflict copies": the Files page on its conflict copies.
    OpenConflicts,
    /// Open the conflict copy at this path (relative to the synced folder) with the OS's default program.
    OpenConflictCopy(String),
    /// Show the folder of the file this conflict copy (at this path) is a copy of.
    ShowConflictOriginal(String),
    /// The home page's "On this computer" section opens or closes.
    ToggleFolders,
    /// Open the file of this app's list at this path (relative to the synced folder) with the OS's default program.
    OpenRecent {
        app: String,
        path: String,
    },
    /// Show the file of this app's list at this path (relative to the synced folder) in its folder, in the file manager.
    ShowRecent {
        app: String,
        path: String,
    },
    /// [`Effect::CheckFile`]'s answer: the path to open (links followed, as it was checked), or why the file can't be opened.
    FileChecked(Result<String, Place>),
    /// [`Effect::OpenFile`]'s outcome: why the OS couldn't be asked, if it couldn't.
    FileOpened(Result<(), String>),
    VersionsPath(String),
    LoadVersions,
    NextVersions,
    BackVersions,
    BackFiles,
    ConfirmRestore(VersionId),
    CancelRestore,
    RestoreVersion,
    LoadTrash,
    /// The trash list's kind filter.
    TrashKind(TrashKind),
    /// The trash list's search field, as typed.
    TrashSearch(String),
    NextTrash,
    BackTrash,
    RestoreTrash(NodeId),
    ConfirmRemoveTrash(NodeId),
    CancelRemoveTrash,
    RemoveTrash,
    AddComputer,
    /// Done or Cancel on the add-a-computer view.
    CloseAdding,
    RemoveDevice(DeviceId),
    /// Rename: the row of this computer becomes a text field.
    StartRename(DeviceId),
    /// Rename: leave the row as it was.
    CancelRename,
    /// Rename: send the typed name.
    SaveRename,
    /// The answer to a rename of this computer (the [`Rename::ticket`] it was sent with): its [`DeviceInfo`] now.
    Renamed(DeviceId, u64, Result<DeviceInfo, Error>),
    /// "Sign out other computers" on the Computers page: opens the Security page at its sign-out step.
    SignOutOtherComputers,
    /// Ask for the computer list again.
    RefreshDevices,
    RefreshUsage,
    /// Ctrl+1 to Ctrl+9: the visible tab at this place in the header (from 1).
    TabNumber(usize),
    /// Ctrl+R: what the page's Refresh does, on a page that has one.
    RefreshPage,
    /// Ctrl+Plus, Ctrl+Minus or Ctrl+0: the interface scale.
    Zoom(ZoomStep),
    /// An edit of the filters of the overview's Recent activity.
    RecentFilter(crate::filter::FilterEdit),
    /// An edit of the filters of the Security page's activity.
    SecurityActivityFilter(crate::filter::FilterEdit),
    SignOut,
    /// Sign out on this computer only (the server can't be reached).
    SignOutHere,
    Pause,
    Resume,
    /// Quit: stops the agent too (until the next login), then the app.
    Quit,
    /// Start the agent again.
    TryAgain,
    /// Try turning start at login on again.
    RetryAutostart,
    /// "Join again" after this computer was removed, or "Sign out" under a problem with the account here: forgets the
    /// account on this computer (the files stay).
    ForgetAccount,
    /// The folder's "Change…".
    ChangeFolder,
    /// The folder picker's answer (`None`: cancelled).
    FolderPicked(Option<String>),
    /// Show the window (a second start, the tray's Open).
    Show,
    /// The window's close button.
    CloseRequested,
    /// Linux: the tray process quit Craft Cloud (it stopped the agent): the window ends too.
    Exit,
    /// A tab of the header: leaves the page shown for that one.
    Tab(Tab),
    /// Something on the admin pages, or the answer to one of their calls ([`crate::admin`]).
    Admin(AdminEvent),
    /// Something on the audit pages (the admin's log, or the account's own activity), or an answer to one of their calls.
    Audit(AuditMsg),
    /// Something on the Shared tab, or the answer to one of its calls ([`crate::spaces`]).
    Spaces(SpacesMsg),
}

/// What to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Find the running agent ([`Event::Found`]).
    Connect,
    /// Start the agent hidden from this app's folder and wait up to 5 s for it ([`Event::AgentStarted`]).
    StartAgent,
    /// A call of the Security page ([`Event::SecurityAnswer`]).
    Security(SecurityCall),
    Status,
    Join(JoinRequest),
    /// The first step of a sign-in with a password ([`Event::LoggedIn`]).
    Login(LoginRequest),
    /// The second step of a sign-in, with the code ([`Event::LoggedIn`]).
    LoginSecondFactor(SecondFactorLoginRequest),
    SetAutostart(bool),
    Pairing,
    PairingState(Uuid),
    CancelPairing(Uuid),
    Devices,
    History {
        path: String,
        cursor: Option<String>,
        ticket: u64,
    },
    Restore {
        path: String,
        version: VersionId,
        expected_version: VersionId,
        ticket: u64,
    },
    Trash {
        cursor: Option<String>,
        ticket: u64,
    },
    RestoreTrash {
        id: NodeId,
        ticket: u64,
    },
    RemoveTrash {
        id: NodeId,
        ticket: u64,
    },
    Usage(u64),
    RemoveDevice(DeviceId),
    RenameDevice {
        id: DeviceId,
        name: String,
        ticket: u64,
    },
    Logout {
        local: bool,
    },
    Pause,
    Resume,
    QuitAgent,
    /// Linux: end the tray process too (Quit); nothing elsewhere, where the tray is this process's.
    QuitTray,
    /// Ask for a folder with the OS's folder picker, starting at this one ([`Event::FolderPicked`]).
    PickFolder(String),
    /// Open the synced folder in the file manager.
    OpenFolder(String),
    /// Look at the file `path` (relative to the synced folder `root`) on the disk once more, now that it is to be opened
    /// ([`crate::files::check`]; [`Event::FileChecked`]). A call off the UI thread, since it reads the disk.
    CheckFile {
        root: String,
        path: String,
    },
    /// Open this file (the path [`Effect::CheckFile`] gave) with the OS's default program ([`Event::FileOpened`]).
    OpenFile(String),
    /// The recent files of every app, placed against the synced folder `root` ([`Event::Recents`]).
    Recents {
        root: Option<String>,
        ticket: u64,
    },
    /// The conflict copies in the synced folder `root`, placed against it ([`Event::Conflicts`]).
    Conflicts {
        root: Option<String>,
        ticket: u64,
    },
    /// The local sizes of the synced folder by folder and kind ([`Event::FolderUsage`]).
    FolderUsage {
        ticket: u64,
    },
    /// End the app (the agent keeps running unless [`Effect::QuitAgent`] came first).
    QuitApp,
    /// Write (`true`) or remove the app's own start-at-login entry.
    AppAutostart(bool),
    /// The signed-in account, as the server says it: how the app finds out whether it is an admin ([`AdminEvent::Account`]).
    Account {
        session: u64,
    },
    /// The newest events of the account's activity, for the home page ([`Event::Recent`]).
    Recent {
        session: u64,
    },
    /// The server's counters, for the admin pages ([`AdminEvent::Server`]).
    AdminServer {
        session: u64,
    },
    /// The server-wide storage overview for the admin pages.
    AdminStorage {
        session: u64,
        ticket: u64,
    },
    /// A page of the user list ([`AdminEvent::Users`] with the same `ticket`).
    AdminUsers {
        query: UserQuery,
        ticket: u64,
    },
    /// A page of the server's team spaces ([`AdminEvent::Spaces`] with the same `ticket`).
    AdminSpaces {
        query: UserQuery,
        ticket: u64,
    },
    /// The account called exactly `name`, if there is one ([`AdminEvent::AccountFound`] with the same `ticket`): the owner a
    /// team space is given.
    AdminFindAccount {
        name: String,
        ticket: u64,
    },
    /// A user's devices ([`AdminEvent::Devices`]).
    AdminDevices {
        user: Uuid,
        ticket: u64,
    },
    /// One account's effective quota and overlapping storage estimates.
    AdminUserUsage {
        user: Uuid,
        ticket: u64,
    },
    /// A page of the audit log or of the account's own activity ([`AuditMsg::Page`] with the same `ticket`).
    AuditPage {
        own: bool,
        query: craft_cloud_app::AuditQuery,
        ticket: u64,
    },
    /// The audit export, written to `path` ([`AuditMsg::Exported`]).
    AuditExport {
        query: craft_cloud_app::AuditQuery,
        path: String,
        ticket: u64,
    },
    /// Ask for the file an export goes to, with the OS's save dialog, starting at this name ([`AuditMsg::ExportPicked`]).
    PickExportFile {
        suggested: String,
        ticket: u64,
    },
    /// A change, with the admin's confirmation ([`AdminEvent::Done`]). Its `Debug` output hides the password.
    AdminDo {
        action: Action,
        confirm: AdminConfirm,
    },
    /// A call of the Shared tab ([`SpacesMsg`]).
    Spaces(SpacesCall),
}

/// Whether the app should start at login, in the tray, given the agent's status: while signed in with the agent
/// starting at login, where the app has a login entry to start into (`entry`: Windows and macOS with a tray icon;
/// Linux always, its entry being the tray process, which waits for a tray to appear). `None` while the agent doesn't
/// say (an agent that doesn't handle start at login): the entry is left as it is.
pub fn app_autostart_wanted(status: &Status, entry: bool) -> Option<bool> {
    status.autostart.map(|on| on && status.signed_in && entry)
}

/// The app's state.
#[derive(Debug, Clone)]
pub struct Model {
    pub screen: Screen,
    /// The agent's last status.
    pub status: Option<Status>,
    /// The time of the last tick.
    pub now_ms: i64,
    /// Whether the window hides into a tray icon of this process when closed (else closing it ends this process; the
    /// agent keeps syncing). Windows and macOS with a tray; never on Linux, whose tray is a process of its own.
    pub tray: bool,
    /// Whether the app has a start-at-login entry ([`app_autostart_wanted`]); the same as `tray` but on Linux, where
    /// it is the tray process's, written whether or not a tray is there right now.
    pub login_entry: bool,
    /// Whether the window is shown.
    pub window_visible: bool,
    /// Start at login couldn't be turned on: said until it is, with Try again.
    pub autostart_problem: Option<String>,
    /// A call that runs once at a time.
    pub busy: Option<Busy>,
    /// Where the agent writes its log, for Details.
    pub log_path: String,
    /// A small note about the app itself (its own start-at-login entry couldn't be written).
    pub note: Option<String>,
    /// What went wrong with the last Sign out, for people; shown on every page of the signed-in window.
    pub sign_out_error: Option<String>,
    /// Signing out failed because the server can't be reached: offer to forget the account here only.
    pub offer_local_sign_out: bool,
    /// The names of the computers and accounts the pages have met, for the IDs events and versions carry.
    pub names: Names,
    defaults: JoinDefaults,
    polling: bool,
    polling_pairing: bool,
    failed_polls: u32,
    /// The app's start-at-login entry as last asked for.
    app_autostart: Option<bool>,
    versions_ticket: u64,
    trash_ticket: u64,
    files_ticket: u64,
    /// The interface scale in tenths (10: 100 %), between [`ZOOM_MIN`] and [`ZOOM_MAX`]. It lasts for the session: the app
    /// keeps no settings of its own, and eframe's storage is not built in.
    zoom_tenths: u8,
    /// The last [`Rename::ticket`] given. It belongs to the model, not to an overview, so that an answer that was on its
    /// way when the overview was replaced can't match a rename of the new one.
    rename_ticket: u64,
    usage_request_id: u64,
    /// What the last admin check said for the sign-in. The pages that don't carry the signed-in view (versions, trash)
    /// read it for the header's Admin tab, and a view made when they are left starts with it.
    pub(crate) access: Access,
}

impl Model {
    pub fn new(defaults: JoinDefaults, tray: bool, log_path: String) -> Self {
        Self {
            screen: Screen::Starting,
            status: None,
            now_ms: 0,
            tray,
            login_entry: tray,
            window_visible: true,
            autostart_problem: None,
            busy: None,
            log_path,
            note: None,
            sign_out_error: None,
            offer_local_sign_out: false,
            defaults,
            polling: false,
            polling_pairing: false,
            failed_polls: 0,
            app_autostart: None,
            versions_ticket: 0,
            trash_ticket: 0,
            files_ticket: 0,
            zoom_tenths: ZOOM_DEFAULT,
            rename_ticket: 0,
            usage_request_id: 0,
            access: Access::Unknown,
            names: Names::default(),
        }
    }

    /// The signed-in view, wherever it is shown.
    pub fn overview(&self) -> Option<&Overview> {
        match &self.screen {
            Screen::SignedIn(overview) => Some(overview),
            Screen::Security(page) => Some(&page.overview),
            Screen::AddingComputer(adding) => Some(&adding.overview),
            Screen::Admin(admin) => Some(&admin.overview),
            Screen::Activity(activity) => Some(&activity.overview),
            Screen::Shared(shared) => Some(&shared.overview),
            _ => None,
        }
    }

    /// Leaves a page for a fresh overview, which asks again for everything it shows: the computers, the admin
    /// check and the storage usage. Without the last two the view would wait for answers nobody asked for.
    fn back_to_overview(&mut self) -> Vec<Effect> {
        let Some(status) = self.status.clone().filter(|s| s.signed_in) else {
            self.screen = Screen::Starting;
            return Vec::new();
        };
        let mut overview = Overview::new(status);
        // Asked again below; until it is answered the tabs keep what was known.
        overview.access = self.access.clone();
        let session = overview.session;
        self.screen = Screen::SignedIn(overview);
        let conflicts = self.overview_mut().map(Overview::ask_conflicts);
        let mut effects = vec![Effect::Devices, Effect::Account { session }, self.next_usage_request(), Effect::Recent { session }];
        effects.extend(conflicts);
        effects
    }

    /// The first start's rail and the step current in it (0 to 2), on the screens of the first start. The code of a
    /// two-factor sign-in and an invite's new password belong to the account step; the join in progress after them as well.
    pub fn first_run_rail(&self) -> Option<(usize, [RailStep; 3])> {
        let (form, current) = match &self.screen {
            Screen::Join(form) => (form, form.step.index()),
            Screen::Joining { form, password: None } => (form, form.step.index()),
            Screen::Joining { form, password: Some(_) } => (form, JoinStep::Account.index()),
            Screen::PasswordNeeded(pw) => (&pw.form, JoinStep::Account.index()),
            Screen::SecondFactor(second) => (&second.form, JoinStep::Account.index()),
            _ => return None,
        };
        Some((current, form.rail_at(current)))
    }

    /// The interface scale for `ctx.set_zoom_factor`: 0.8 to 1.6.
    pub fn zoom_factor(&self) -> f32 {
        f32::from(self.zoom_tenths) / 10.0
    }

    /// The tab of the page shown, in the signed-in window.
    pub fn tab(&self) -> Option<Tab> {
        match &self.screen {
            Screen::SignedIn(o) => Some(match o.page {
                OverviewPage::Home => Tab::Overview,
                OverviewPage::Computers => Tab::Computers,
            }),
            Screen::AddingComputer(_) => Some(Tab::Computers),
            Screen::Versions(_) => Some(Tab::Versions),
            Screen::Trash(_) => Some(Tab::Trash),
            Screen::Files(_) => Some(Tab::Files),
            Screen::Shared(_) => Some(Tab::Shared),
            Screen::Security(_) => Some(Tab::Security),
            Screen::Activity(_) => Some(Tab::Activity),
            Screen::Admin(_) => Some(Tab::Admin),
            _ => None,
        }
    }

    /// The header's tabs: all the pages, the admin pages for an admin only.
    pub fn tabs(&self) -> Vec<Tab> {
        let all = [Tab::Overview, Tab::Files, Tab::Shared, Tab::Versions, Tab::Trash, Tab::Computers, Tab::Security, Tab::Activity, Tab::Admin];
        all.into_iter().filter(|t| *t != Tab::Admin || self.is_admin()).collect()
    }

    /// Whether the page shown can't be left by a tab now: a call of the security page is under way or it shows one-time
    /// recovery codes, or the admin pages wait for a call's answer or show a one-time code. Leaving would drop what the
    /// answer or the screen holds.
    pub fn tabs_locked(&self) -> bool {
        match &self.screen {
            Screen::Security(page) => page.busy || matches!(page.step, SecurityStep::Codes { .. }),
            Screen::Admin(admin) => admin.code.is_some() || admin.dialog.as_ref().is_some_and(|d| d.busy),
            _ => false,
        }
    }

    /// The event of the Refresh button of the page shown, if the page has one.
    fn refresh_event(&self) -> Option<Event> {
        match &self.screen {
            Screen::SignedIn(o) => Some(match o.page {
                OverviewPage::Home => Event::RefreshUsage,
                OverviewPage::Computers => Event::RefreshDevices,
            }),
            Screen::Trash(_) => Some(Event::LoadTrash),
            Screen::Files(_) => Some(Event::LoadFiles),
            Screen::Shared(_) => Some(Event::Spaces(SpacesMsg::Reload)),
            Screen::Activity(_) => Some(Event::Audit(AuditMsg::ActivityReload)),
            // A code or the password page covers the page, and the new account's form has no Refresh.
            Screen::Admin(admin) if admin.code.is_none() && admin.dialog.is_none() => match admin.view {
                crate::admin::View::List => Some(Event::Admin(AdminEvent::Reload)),
                crate::admin::View::Audit(_) => Some(Event::Audit(AuditMsg::Reload)),
                crate::admin::View::User(_) => Some(Event::Admin(AdminEvent::RefreshUsage)),
                crate::admin::View::Create(_) | crate::admin::View::CreateSpace(_) => None,
                crate::admin::View::Space(_) => Some(Event::Admin(AdminEvent::Reload)),
            },
            _ => None,
        }
    }

    /// Whether the page shown has a search field now (Ctrl+F goes to it).
    pub fn has_search(&self) -> bool {
        match &self.screen {
            Screen::Files(files) => files.view == FilesView::Recent && files.list.as_ref().is_some_and(|list| !list.is_empty()),
            Screen::Trash(trash) => trash.page.is_some(),
            Screen::SignedIn(o) => o.page == OverviewPage::Home && matches!(&o.recent, Recent::Loaded(events) if !events.is_empty()),
            Screen::Security(page) => {
                matches!(page.step, SecurityStep::Home | SecurityStep::SignOutOthers)
                    && matches!(&page.activity, crate::security::SecurityActivity::Loaded(events) if !events.is_empty())
            }
            Screen::Admin(admin) => {
                admin.code.is_none()
                    && admin.dialog.is_none()
                    && matches!(admin.view, crate::admin::View::List)
                    && admin.section != crate::admin::Section::Storage
            }
            _ => false,
        }
    }

    /// A tab of the header: the page shown is left as its own way back would (a computer code still waiting is
    /// cancelled, what the security page showed dropped, the tickets of versions and trash answers moved on), then the
    /// tab's page is opened as the button that opened it did.
    fn open_tab(&mut self, tab: Tab) -> Vec<Effect> {
        let Some(current) = self.tab() else { return Vec::new() };
        if current == tab || self.tabs_locked() || (tab == Tab::Admin && !self.is_admin()) {
            return Vec::new();
        }
        let mut effects = match &self.screen {
            Screen::Versions(_) => self.update(Event::BackVersions),
            Screen::Trash(_) => self.update(Event::BackTrash),
            Screen::Files(_) => self.update(Event::BackFiles),
            Screen::Shared(_) => self.spaces(SpacesMsg::Close),
            Screen::Security(_) => self.security(SecurityEvent::Close),
            Screen::Activity(_) => self.audit(AuditMsg::CloseActivity),
            Screen::Admin(_) => self.admin(AdminEvent::Close),
            Screen::AddingComputer(_) => self.update(Event::CloseAdding),
            _ => Vec::new(),
        };
        let Screen::SignedIn(overview) = &mut self.screen else { return effects };
        overview.page = if tab == Tab::Computers { OverviewPage::Computers } else { OverviewPage::Home };
        // A name being typed is dropped with the page; one on its way keeps its ticket.
        if overview.renaming.as_ref().is_some_and(|r| !r.busy) {
            overview.renaming = None;
        }
        effects.extend(match tab {
            Tab::Overview | Tab::Computers => Vec::new(),
            Tab::Versions => self.update(Event::OpenVersions),
            Tab::Trash => self.update(Event::OpenTrash),
            Tab::Files => self.update(Event::OpenFiles),
            Tab::Shared => self.spaces(SpacesMsg::Open),
            Tab::Security => self.update(Event::OpenSecurity),
            Tab::Activity => self.audit(AuditMsg::OpenActivity),
            Tab::Admin => self.admin(AdminEvent::Open),
        });
        effects
    }

    fn next_usage_request(&mut self) -> Effect {
        self.usage_request_id = self.usage_request_id.wrapping_add(1);
        Effect::Usage(self.usage_request_id)
    }

    pub(crate) fn overview_mut(&mut self) -> Option<&mut Overview> {
        match &mut self.screen {
            Screen::SignedIn(overview) => Some(overview),
            Screen::Security(page) => Some(&mut page.overview),
            Screen::AddingComputer(adding) => Some(&mut adding.overview),
            Screen::Admin(admin) => Some(&mut admin.overview),
            Screen::Activity(activity) => Some(&mut activity.overview),
            Screen::Shared(shared) => Some(&mut shared.overview),
            _ => None,
        }
    }

    pub fn update(&mut self, event: Event) -> Vec<Effect> {
        if self.asks_to_discard(&event) {
            return Vec::new();
        }
        match event {
            Event::Start => {
                self.screen = Screen::Starting;
                vec![Effect::Connect]
            }
            Event::Tick { now_ms } => {
                self.now_ms = now_ms;
                self.tick()
            }
            Event::Found(Ok(status)) | Event::AgentStarted(Ok(status)) => self.status_came(status),
            Event::Found(Err(Error::Version(v))) => {
                self.version_problem(v);
                Vec::new()
            }
            Event::Found(Err(_)) => {
                self.screen = Screen::Starting;
                vec![Effect::StartAgent]
            }
            Event::AgentStarted(Err(why)) => {
                self.screen = Screen::NotRunning(self.not_running(DIDNT_START, &why));
                Vec::new()
            }
            Event::Polled(result) => {
                self.polling = false;
                match result {
                    Ok(status) => {
                        self.failed_polls = 0;
                        self.status_came(status)
                    }
                    Err(e) => {
                        self.poll_failed(e);
                        Vec::new()
                    }
                }
            }
            Event::Joined(result) | Event::LoggedIn(result) => self.joined(result),
            Event::AutostartSet(Ok(status)) => {
                self.autostart_problem = (status.autostart == Some(false)).then(|| NO_SYNC_AFTER_RESTART.to_owned());
                self.status_came(status)
            }
            Event::AutostartSet(Err(e)) => {
                self.autostart_problem = Some(format!("{NO_SYNC_AFTER_RESTART} ({})", message(&e)));
                Vec::new()
            }
            Event::PairingMade(result) => {
                if let Screen::AddingComputer(adding) = &mut self.screen
                    && adding.state == AddState::Asking
                {
                    match result {
                        Ok(info) => {
                            adding.info = Some(info);
                            adding.state = AddState::Waiting;
                        }
                        Err(e) => adding.state = AddState::Failed(message(&e)),
                    }
                }
                Vec::new()
            }
            Event::PairingPolled(result) => self.pairing_polled(result),
            Event::Devices(result) => {
                if let Ok(list) = &result {
                    self.names.learn_computers(list);
                }
                if let Some(overview) = self.overview_mut() {
                    overview.removing = None;
                    match result {
                        Ok(list) => {
                            // A computer that is gone can't be renamed any more.
                            if overview.renaming.as_ref().is_some_and(|r| !list.iter().any(|d| d.id == r.id && !d.revoked)) {
                                overview.renaming = None;
                            }
                            overview.devices = Some(list);
                            overview.devices_error = None;
                        }
                        Err(e) => overview.devices_error = Some(message(&e)),
                    }
                }
                Vec::new()
            }
            Event::History { ticket, result } => {
                let Screen::Versions(versions) = &mut self.screen else { return Vec::new() };
                if versions.ticket != ticket {
                    return Vec::new();
                }
                versions.loading = false;
                match result {
                    Ok(page) => {
                        if versions.current_version.is_none() {
                            versions.current_version = page.versions.first().map(|row| row.id);
                        }
                        versions.page = Some(page);
                        versions.error = None;
                    }
                    Err(e) => versions.error = Some(message(&e)),
                }
                Vec::new()
            }
            Event::Restored { ticket, result } => {
                let Screen::Versions(versions) = &mut self.screen else { return Vec::new() };
                if versions.ticket != ticket {
                    return Vec::new();
                }
                versions.restoring = false;
                versions.confirm = None;
                match result {
                    Ok(_) => {
                        versions.notice = Some("Version restored. Sync will update this file.".into());
                        versions.current_version = None;
                        self.request_history(None)
                    }
                    Err(Error::Refused { code, .. }) if code == api_codes::CONFLICT => {
                        versions.notice = Some("The file changed meanwhile. Version list reloaded.".into());
                        versions.current_version = None;
                        self.request_history(None)
                    }
                    Err(Error::Refused { code, .. }) if code == api_codes::INVALID => {
                        versions.error = Some("This file can't be restored: it may be a live document or deleted.".into());
                        Vec::new()
                    }
                    Err(e) => {
                        versions.error = Some(message(&e));
                        Vec::new()
                    }
                }
            }
            Event::TrashPage { ticket, result } => {
                let Screen::Trash(trash) = &mut self.screen else { return Vec::new() };
                if trash.ticket != ticket {
                    return Vec::new();
                }
                trash.loading = false;
                match result {
                    Ok(page) => {
                        trash.page = Some(page);
                        trash.error = None;
                    }
                    Err(e) => trash.error = Some(message(&e)),
                }
                Vec::new()
            }
            Event::TrashRestored { ticket, result } => {
                let Screen::Trash(trash) = &mut self.screen else { return Vec::new() };
                if trash.ticket != ticket {
                    return Vec::new();
                }
                let prior = trash.action.take();
                match result {
                    Ok(response) => {
                        let (path, parent_path) = prior
                            .and_then(|action| match action {
                                TrashAction::Restore { path, parent_path, .. } => Some((path, parent_path)),
                                _ => None,
                            })
                            .unwrap_or((None, None));
                        trash.notice = Some(if response.renamed {
                            match parent_path {
                                Some(parent) if parent.is_empty() => format!("Restored as {} in the sync folder.", response.name),
                                Some(parent) => format!("Restored as {} in {}.", response.name, parent),
                                None => format!("Restored as {}. The destination path is unavailable.", response.name),
                            }
                        } else {
                            match path {
                                Some(path) => format!("Restored to {}.", path),
                                None => format!("Restored {}. The destination path is unavailable.", response.name),
                            }
                        });
                        self.request_trash(None)
                    }
                    Err(Error::Refused { code, name_taken, .. }) if code == api_codes::CONFLICT => {
                        trash.error = Some(if name_taken == Some(true) {
                            "This item couldn't be restored because another item already has that name.".into()
                        } else {
                            "This item couldn't be restored because it conflicts with the current folder contents.".into()
                        });
                        Vec::new()
                    }
                    Err(e) => {
                        trash.error = Some(message(&e));
                        Vec::new()
                    }
                }
            }
            Event::TrashRemoved { ticket, result } => {
                let Screen::Trash(trash) = &mut self.screen else { return Vec::new() };
                if trash.ticket != ticket {
                    return Vec::new();
                }
                trash.action = None;
                match result {
                    Ok(()) => {
                        trash.notice = Some("Removed from trash. It can no longer be restored from the trash.".into());
                        self.request_trash(None)
                    }
                    Err(e) => {
                        trash.error = Some(message(&e));
                        Vec::new()
                    }
                }
            }
            Event::Recent { session, result } => {
                if let Some(overview) = self.overview_mut().filter(|o| o.session == session) {
                    overview.recent = match result {
                        Ok(page) => Recent::Loaded(page.events.into_iter().take(RECENT_ACTIVITY as usize).collect()),
                        Err(e) => Recent::Failed(message(&e)),
                    };
                }
                Vec::new()
            }
            Event::Usage { request_id, result } => {
                if request_id != self.usage_request_id {
                    return Vec::new();
                }
                if let Some(overview) = self.overview_mut() {
                    overview.usage_loading = false;
                    match result {
                        Ok(usage) => {
                            overview.usage = Some(usage);
                            overview.usage_error = None;
                        }
                        Err(e) => overview.usage_error = Some(message(&e)),
                    }
                }
                Vec::new()
            }
            Event::SignedOut(result) => self.signed_out(result),
            Event::Answered(result) => {
                self.busy = self.busy.filter(|b| *b != Busy::Pausing);
                match result {
                    Ok(status) => self.status_came(status),
                    Err(e) => {
                        if let Some(overview) = self.overview_mut() {
                            overview.error = Some(message(&e));
                        }
                        Vec::new()
                    }
                }
            }
            Event::AgentQuit => vec![Effect::QuitApp],
            Event::AppAutostartSet(result) => {
                self.note = result.err().map(|why| format!("Craft Cloud won't open by itself at login: {why}"));
                Vec::new()
            }

            Event::OpenSecurity => {
                let Screen::SignedIn(overview) = &self.screen else { return Vec::new() };
                let (page, calls) = SecurityPage::open(overview.clone());
                self.screen = Screen::Security(Box::new(page));
                calls.into_iter().map(Effect::Security).collect()
            }
            Event::Security(event) => self.security(event),
            Event::SecurityAnswer(answer) => {
                let now_ms = self.now_ms;
                let Screen::Security(page) = &mut self.screen else { return Vec::new() };
                // These sign the account's other computers out: the list the page shows is stale after them.
                let signs_out =
                    matches!(&answer, SecurityAnswer::SignOutOthers(Ok(_)) | SecurityAnswer::ChangePassword(Ok(_)) | SecurityAnswer::Disable(Ok(_)));
                let mut effects: Vec<Effect> = page.answer(answer, now_ms).into_iter().map(Effect::Security).collect();
                if signs_out {
                    effects.push(Effect::Devices);
                }
                effects
            }
            Event::SubmitSecondFactor => self.submit_second_factor(),
            Event::CancelSecondFactor => {
                if let Screen::SecondFactor(form) = &self.screen {
                    self.screen = match &form.password {
                        Some(pw) => Screen::PasswordNeeded(pw.clone()),
                        None => {
                            // A sign-in's password is not kept: it is typed again on the account step.
                            let step = if form.form.mode == JoinMode::SignIn { JoinStep::Account } else { form.form.step };
                            Screen::Join(JoinForm { challenge: None, password: String::new(), step, ..form.form.clone() })
                        }
                    };
                }
                Vec::new()
            }
            Event::SubmitJoin => self.submit_join(),
            Event::JoinNext => self.join_next(),
            Event::JoinBack => {
                if let Screen::Join(form) = &mut self.screen
                    && let Some(step) = form.step.prev()
                {
                    form.step = step;
                    form.error = None;
                    form.ask_adopt = false;
                }
                Vec::new()
            }
            Event::SetJoinMode(mode) => {
                if let Screen::Join(form) = &mut self.screen
                    && form.mode != mode
                {
                    // The folder and the computer's name stay; what belonged to the other way does not.
                    form.mode = mode;
                    form.password.clear();
                    form.error = None;
                    form.ask_adopt = false;
                }
                Vec::new()
            }
            Event::SyncExistingFiles(yes) => {
                let Screen::Join(form) = &mut self.screen else { return Vec::new() };
                form.ask_adopt = false;
                if !yes {
                    return Vec::new();
                }
                form.adopt_for = Some(form.folder.trim().to_owned());
                if form.mode == JoinMode::SignIn && form.password.is_empty() {
                    // The password went with the first try and is not kept.
                    form.error = Some(PASSWORD_AGAIN.to_owned());
                    form.step = JoinStep::Account;
                    return Vec::new();
                }
                self.submit_join()
            }
            Event::SubmitPassword => self.submit_password(),
            Event::CancelPassword => {
                if let Screen::PasswordNeeded(pw) = &self.screen {
                    self.screen = Screen::Join(pw.form.clone());
                }
                Vec::new()
            }
            Event::OpenFolder => self.status.as_ref().and_then(|s| s.folder.clone()).map(Effect::OpenFolder).into_iter().collect(),
            Event::OpenVersions => {
                if !matches!(self.screen, Screen::SignedIn(_)) {
                    return Vec::new();
                }
                self.screen = Screen::Versions(Versions {
                    path: String::new(),
                    page: None,
                    current_version: None,
                    loading: false,
                    restoring: false,
                    error: None,
                    notice: None,
                    confirm: None,
                    ticket: 0,
                });
                Vec::new()
            }
            Event::OpenTrash => {
                if !matches!(self.screen, Screen::SignedIn(_)) {
                    return Vec::new();
                }
                self.screen = Screen::Trash(Trash::new());
                self.request_trash(None)
            }
            Event::TrashKind(kind) => {
                if let Screen::Trash(t) = &mut self.screen {
                    t.kind = kind;
                }
                Vec::new()
            }
            Event::TrashSearch(search) => {
                if let Screen::Trash(t) = &mut self.screen {
                    t.search = search;
                }
                Vec::new()
            }
            Event::OpenFiles => {
                if !matches!(self.screen, Screen::SignedIn(_)) {
                    return Vec::new();
                }
                self.screen = Screen::Files(Files::default());
                self.request_files()
            }
            Event::LoadFiles => self.request_files(),
            Event::FilesApp(app) => {
                if let Screen::Files(f) = &mut self.screen {
                    f.app = app;
                }
                Vec::new()
            }
            Event::FilesSearch(search) => {
                if let Screen::Files(f) = &mut self.screen {
                    f.search = search;
                }
                Vec::new()
            }
            Event::BackFiles => {
                if matches!(self.screen, Screen::Files(_)) {
                    self.files_ticket = self.files_ticket.saturating_add(1);
                    return self.back_to_overview();
                }
                Vec::new()
            }
            Event::Recents { ticket, result } => {
                let Screen::Files(files) = &mut self.screen else { return Vec::new() };
                if files.ticket != ticket {
                    return Vec::new();
                }
                files.loading = false;
                match result {
                    Ok(list) => {
                        files.list = Some(list);
                        files.error = None;
                        // A filter for an app that has no files any more would show nothing and offer no way back.
                        if files.app.is_some() && !files.counts().1.iter().any(|(app, _)| Some(app) == files.app.as_ref()) {
                            files.app = None;
                        }
                    }
                    Err(e) => files.error = Some(message(&e)),
                }
                Vec::new()
            }
            Event::Conflicts { ticket, result } => {
                match &mut self.screen {
                    Screen::Files(files) => {
                        if files.conflicts.ticket == ticket {
                            files.conflicts.loading = false;
                            match result {
                                Ok(list) => {
                                    files.conflicts.list = Some(list);
                                    files.conflicts.error = None;
                                }
                                Err(e) => files.conflicts.error = Some(message(&e)),
                            }
                        }
                    }
                    _ => {
                        // The home page only wants the count; a list it couldn't get is no news worth a line.
                        if let Some(overview) = self.overview_mut().filter(|o| o.conflicts_ticket == ticket) {
                            overview.conflicts = result.ok().map(|list| ConflictSummary { count: list.files.len(), truncated: list.truncated });
                        }
                    }
                }
                Vec::new()
            }
            Event::FolderUsage { ticket, result } => {
                // The overview may be under another screen (the Security page, adding a computer, the admin pages, the
                // activity): the answer is for it all the same, or its spinner would never stop.
                if let Some(overview) = self.overview_mut()
                    && overview.folders.ticket == ticket
                {
                    overview.folders.loading = false;
                    match result {
                        Ok(data) => {
                            overview.folders.data = Some(data);
                            overview.folders.error = None;
                        }
                        Err(e) => overview.folders.error = Some(message(&e)),
                    }
                }
                Vec::new()
            }
            Event::ToggleFolders => {
                let Screen::SignedIn(overview) = &mut self.screen else { return Vec::new() };
                if overview.page != OverviewPage::Home {
                    return Vec::new();
                }
                overview.folders.open = !overview.folders.open;
                if overview.folders.open && overview.folders.data.is_none() { overview.ask_folders().into_iter().collect() } else { Vec::new() }
            }
            Event::FilesView(view) => {
                if let Screen::Files(files) = &mut self.screen {
                    files.view = view;
                }
                Vec::new()
            }
            Event::OpenConflicts => {
                let effects = self.update(Event::Tab(Tab::Files));
                if let Screen::Files(files) = &mut self.screen {
                    files.view = FilesView::Conflicts;
                }
                effects
            }
            Event::OpenConflictCopy(path) => self.open_conflict(&path, false),
            Event::ShowConflictOriginal(path) => self.open_conflict(&path, true),
            Event::OpenRecent { app, path } => self.open_recent(&app, &path),
            Event::ShowRecent { app, path } => self.show_recent(&app, &path),
            Event::FileChecked(result) => match result {
                Ok(full) => vec![Effect::OpenFile(full)],
                Err(place) => {
                    if let Screen::Files(files) = &mut self.screen {
                        let why = place.reason().map(|why| format!("Can't open this file: {why}."));
                        // The page says it where the Open was clicked: with the conflict copies, or the recent files.
                        if files.view == FilesView::Conflicts {
                            files.conflicts.open_error = why;
                        } else {
                            files.open_error = why;
                        }
                    }
                    Vec::new()
                }
            },
            Event::FileOpened(result) => {
                if let (Screen::Files(files), Err(why)) = (&mut self.screen, result) {
                    files.open_error = Some(format!("Couldn't open the file: {why}"));
                }
                Vec::new()
            }
            Event::LoadTrash => self.request_trash(None),
            Event::NextTrash => {
                let cursor = match &self.screen {
                    Screen::Trash(t) if t.action.is_none() && !t.loading => t.page.as_ref().and_then(|p| p.next.clone()),
                    _ => None,
                };
                cursor.map(|cursor| self.request_trash(Some(cursor))).unwrap_or_default()
            }
            Event::BackTrash => {
                if matches!(self.screen, Screen::Trash(_)) {
                    self.trash_ticket = self.trash_ticket.saturating_add(1);
                    return self.back_to_overview();
                }
                Vec::new()
            }
            Event::RestoreTrash(id) => {
                let Screen::Trash(t) = &mut self.screen else { return Vec::new() };
                if t.loading || t.action.is_some() {
                    return Vec::new();
                }
                let Some(item) = t.page.as_ref().and_then(|p| p.items.iter().find(|item| item.id == id)) else { return Vec::new() };
                let path = item.path.clone();
                let parent_path = path.as_deref().map(|p| p.rsplit_once('/').map(|(parent, _)| parent.to_owned()).unwrap_or_default());
                self.begin_trash_action(id, TrashAction::Restore { path, parent_path })
            }
            Event::ConfirmRemoveTrash(id) => {
                if let Screen::Trash(t) = &mut self.screen
                    && !t.loading
                    && t.action.is_none()
                    && t.page.as_ref().is_some_and(|p| p.items.iter().any(|i| i.id == id))
                {
                    t.confirm = Some(id);
                }
                Vec::new()
            }
            Event::CancelRemoveTrash => {
                if let Screen::Trash(t) = &mut self.screen {
                    t.confirm = None;
                }
                Vec::new()
            }
            Event::RemoveTrash => {
                let Screen::Trash(t) = &mut self.screen else { return Vec::new() };
                if t.loading || t.action.is_some() {
                    return Vec::new();
                }
                let Some(id) = t.confirm.take() else { return Vec::new() };
                self.begin_trash_action(id, TrashAction::Remove)
            }
            Event::VersionsPath(path) => {
                if let Screen::Versions(v) = &mut self.screen {
                    if v.path != path {
                        self.versions_ticket = self.versions_ticket.saturating_add(1);
                        v.ticket = self.versions_ticket;
                        v.page = None;
                        v.current_version = None;
                        v.loading = false;
                        v.restoring = false;
                        v.confirm = None;
                        v.notice = None;
                    }
                    v.path = path;
                    v.error = None;
                }
                Vec::new()
            }
            Event::LoadVersions => {
                if let Screen::Versions(v) = &mut self.screen {
                    if v.restoring || v.loading {
                        return Vec::new();
                    }
                    v.notice = None;
                    v.current_version = None;
                }
                self.request_history(None)
            }
            Event::NextVersions => {
                let cursor = match &self.screen {
                    Screen::Versions(v) => v.page.as_ref().and_then(|p| p.next.clone()),
                    _ => None,
                };
                cursor.map(|c| self.request_history(Some(c))).unwrap_or_default()
            }
            Event::BackVersions => {
                if let Screen::Versions(_) = self.screen {
                    self.versions_ticket = self.versions_ticket.saturating_add(1);
                    return self.back_to_overview();
                }
                Vec::new()
            }
            Event::ConfirmRestore(version) => {
                if let Screen::Versions(v) = &mut self.screen
                    && !v.restoring
                    && !v.loading
                    && Some(version) != v.current_version
                    && v.page.as_ref().is_some_and(|page| page.versions.iter().any(|row| row.id == version && !row.pruned))
                {
                    v.confirm = Some(version);
                }
                Vec::new()
            }
            Event::CancelRestore => {
                if let Screen::Versions(v) = &mut self.screen {
                    v.confirm = None;
                }
                Vec::new()
            }
            Event::RestoreVersion => self.restore_version(),
            Event::AddComputer => {
                let Screen::SignedIn(overview) = &self.screen else { return Vec::new() };
                self.screen = Screen::AddingComputer(Adding { overview: overview.clone(), info: None, state: AddState::Asking });
                vec![Effect::Pairing]
            }
            Event::CloseAdding => {
                let Screen::AddingComputer(adding) = &self.screen else { return Vec::new() };
                let mut effects = Vec::new();
                if let (AddState::Waiting, Some(info)) = (&adding.state, &adding.info) {
                    effects.push(Effect::CancelPairing(info.id));
                }
                self.screen = Screen::SignedIn(adding.overview.restored());
                self.polling_pairing = false;
                effects.push(Effect::Devices);
                effects
            }
            Event::RemoveDevice(id) => {
                let Some(overview) = self.overview_mut() else { return Vec::new() };
                let removable = overview.devices.iter().flatten().any(|d| d.id == id && !d.current);
                if overview.removing.is_some() || !removable {
                    return Vec::new();
                }
                overview.removing = Some(id);
                vec![Effect::RemoveDevice(id)]
            }
            Event::StartRename(id) => {
                let Some(overview) = self.overview_mut() else { return Vec::new() };
                if overview.renaming.as_ref().is_some_and(|r| r.busy) {
                    return Vec::new();
                }
                if let Some(device) = overview.devices.iter().flatten().find(|d| d.id == id && !d.revoked) {
                    overview.renaming = Some(Rename { id, ticket: 0, name: device.name.clone(), busy: false, error: None });
                }
                Vec::new()
            }
            Event::CancelRename => {
                if let Some(overview) = self.overview_mut()
                    && overview.renaming.as_ref().is_some_and(|r| !r.busy)
                {
                    overview.renaming = None;
                }
                Vec::new()
            }
            Event::SaveRename => {
                let ticket = self.rename_ticket.wrapping_add(1);
                let Some(overview) = self.overview_mut() else { return Vec::new() };
                let Some(rename) = overview.renaming.as_mut().filter(|r| !r.busy) else { return Vec::new() };
                let name = match check_device_name(&rename.name) {
                    Ok(name) => name,
                    Err(why) => {
                        rename.error = Some(why.to_owned());
                        return Vec::new();
                    }
                };
                let id = rename.id;
                // Nothing to send when the name is the one it has.
                if overview.devices.iter().flatten().any(|d| d.id == id && d.name == name) {
                    overview.renaming = None;
                    return Vec::new();
                }
                rename.error = None;
                rename.busy = true;
                if let Some(rename) = overview.renaming.as_mut() {
                    rename.ticket = ticket;
                }
                self.rename_ticket = ticket;
                vec![Effect::RenameDevice { id, name, ticket }]
            }
            Event::Renamed(id, ticket, result) => {
                let Some(overview) = self.overview_mut() else { return Vec::new() };
                let current = overview.renaming.as_ref().is_some_and(|r| r.id == id && r.ticket == ticket);
                let mut effects = Vec::new();
                match result {
                    Ok(renamed) => {
                        // This computer's own name is also the agent's status (the overview's title): ask for it again.
                        if renamed.current {
                            effects.push(Effect::Status);
                        }
                        // The server's answer is the computer's name now, whether its field is still open or not.
                        if let Some(device) = overview.devices.iter_mut().flatten().find(|d| d.id == renamed.id) {
                            *device = renamed;
                        }
                        if current {
                            overview.renaming = None;
                        }
                    }
                    Err(e) => {
                        if current && let Some(rename) = overview.renaming.as_mut() {
                            rename.busy = false;
                            rename.error = Some(message(&e));
                        }
                    }
                }
                effects
            }
            Event::SignOutOtherComputers => {
                if !matches!(self.screen, Screen::SignedIn(_)) {
                    return Vec::new();
                }
                if let Some(overview) = self.overview_mut() {
                    overview.renaming = None;
                }
                let mut effects = self.update(Event::OpenSecurity);
                effects.extend(self.security(SecurityEvent::StartSignOutOthers));
                effects
            }
            Event::RefreshDevices => {
                let Some(overview) = self.overview() else { return Vec::new() };
                // An admin check that couldn't be made at sign-in is made again.
                let again = (overview.access == Access::Unknown).then_some(Effect::Account { session: overview.session });
                std::iter::once(Effect::Devices).chain(again).collect()
            }
            Event::Tab(tab) => self.open_tab(tab),
            Event::TabNumber(number) => match number.checked_sub(1).and_then(|index| self.tabs().get(index).copied()) {
                Some(tab) => self.update(Event::Tab(tab)),
                None => Vec::new(),
            },
            Event::Zoom(step) => {
                self.zoom_tenths = match step {
                    ZoomStep::In => self.zoom_tenths.saturating_add(1).min(ZOOM_MAX),
                    ZoomStep::Out => self.zoom_tenths.saturating_sub(1).max(ZOOM_MIN),
                    ZoomStep::Reset => ZOOM_DEFAULT,
                };
                Vec::new()
            }
            Event::RecentFilter(edit) => {
                if let Screen::SignedIn(overview) = &mut self.screen {
                    overview.recent_filter.edit(edit);
                }
                Vec::new()
            }
            Event::SecurityActivityFilter(edit) => {
                if let Screen::Security(page) = &mut self.screen {
                    page.activity_filter.edit(edit);
                }
                Vec::new()
            }
            Event::RefreshPage => match self.refresh_event() {
                Some(refresh) => self.update(refresh),
                None => Vec::new(),
            },
            Event::Admin(event) => self.admin(event),
            Event::Audit(msg) => self.audit(msg),
            Event::Spaces(msg) => self.spaces(msg),
            Event::RefreshUsage => {
                let Some(overview) = self.overview_mut() else { return Vec::new() };
                if overview.usage_loading {
                    return Vec::new();
                }
                overview.usage_loading = true;
                overview.usage_error = None;
                // The sizes by folder and the conflict copies come from the agent's saved state, so they are read again with the usage.
                let conflicts = overview.ask_conflicts();
                let folders = if overview.folders.open { overview.ask_folders() } else { None };
                let mut effects = vec![self.next_usage_request(), conflicts];
                effects.extend(folders);
                effects
            }
            Event::SignOut => self.sign_out(false),
            Event::SignOutHere => self.sign_out(true),
            Event::Pause | Event::Resume => {
                let signed_in = self.status.as_ref().is_some_and(|s| s.signed_in);
                if self.busy.is_some() || !signed_in {
                    return Vec::new();
                }
                self.busy = Some(Busy::Pausing);
                vec![if event == Event::Pause { Effect::Pause } else { Effect::Resume }]
            }
            Event::Quit => {
                if self.busy == Some(Busy::Quitting) || self.tabs_locked() {
                    return Vec::new();
                }
                self.busy = Some(Busy::Quitting);
                vec![Effect::QuitTray, Effect::QuitAgent]
            }
            Event::Exit => vec![Effect::QuitApp],
            Event::TryAgain => {
                if !matches!(self.screen, Screen::NotRunning(_)) {
                    return Vec::new();
                }
                self.screen = Screen::Starting;
                vec![Effect::StartAgent]
            }
            Event::RetryAutostart => {
                self.autostart_problem = None;
                vec![Effect::SetAutostart(true)]
            }
            Event::ForgetAccount => {
                let stuck = matches!(self.screen, Screen::Removed | Screen::Error(Problem { action: ProblemAction::ForgetAccount, .. }));
                if !stuck || self.busy.is_some() {
                    return Vec::new();
                }
                self.busy = Some(Busy::SigningOut);
                vec![Effect::Logout { local: true }]
            }
            Event::ChangeFolder => match &self.screen {
                Screen::Join(form) => vec![Effect::PickFolder(form.folder.clone())],
                _ => Vec::new(),
            },
            Event::FolderPicked(picked) if matches!(self.screen, Screen::Shared(_)) => self.spaces(SpacesMsg::FolderPicked(picked)),
            Event::FolderPicked(picked) => {
                if let (Screen::Join(form), Some(folder)) = (&mut self.screen, picked) {
                    form.folder = folder;
                    form.ask_adopt = false;
                    form.error = None;
                }
                Vec::new()
            }
            Event::Show => {
                self.window_visible = true;
                Vec::new()
            }
            Event::CloseRequested => {
                if self.tray {
                    self.window_visible = false;
                    Vec::new()
                } else {
                    // No tray to live in: the app ends; the agent keeps syncing.
                    vec![Effect::QuitApp]
                }
            }
        }
    }

    /// Whether `event` would drop role or quota edits of an account's admin page that were not saved. It is then not carried
    /// out (the app ending too: a close without a tray, the Linux tray's Exit): the page asks "Discard the unsaved changes?" and [`AdminEvent::DiscardEdits`] carries it out afterwards, while
    /// [`AdminEvent::KeepEditing`] drops it. The window is shown for the question (a Quit from the tray comes with it hidden).
    fn asks_to_discard(&mut self, event: &Event) -> bool {
        let locked = self.tabs_locked();
        let current = self.tab();
        let Screen::Admin(admin) = &self.screen else { return false };
        let no_dialog = admin.dialog.is_none();
        let leaves = match event {
            Event::Tab(tab) => Some(*tab) != current && !locked,
            Event::SignOut | Event::SignOutHere | Event::Quit => !locked,
            // The window's close button without a tray, and the Linux tray process's Quit, end the app and the edits with it.
            Event::CloseRequested | Event::Exit => true,
            Event::Admin(AdminEvent::Close) => true,
            Event::Admin(AdminEvent::BackToList) | Event::Audit(AuditMsg::OpenAudit(_)) => no_dialog,
            Event::Admin(AdminEvent::OpenUser(id)) => no_dialog && !matches!(&admin.view, crate::admin::View::User(view) if view.id == *id),
            _ => false,
        };
        if !leaves || !admin.has_unsaved_edits() {
            return false;
        }
        if let Screen::Admin(admin) = &mut self.screen {
            admin.leaving = Some(Box::new(event.clone()));
        }
        self.window_visible = true;
        true
    }

    /// Every 2 s: the agent's status (one call at a time), and an open add-a-computer code's state.
    fn tick(&mut self) -> Vec<Effect> {
        let mut effects = Vec::new();
        if matches!(self.screen, Screen::Starting) || self.busy == Some(Busy::Quitting) {
            return effects;
        }
        if !self.polling {
            self.polling = true;
            effects.push(Effect::Status);
        }
        let now = self.now_ms;
        if let Screen::AddingComputer(adding) = &mut self.screen
            && adding.state == AddState::Waiting
            && let Some(info) = &adding.info
        {
            if now >= info.expires_ms {
                adding.state = AddState::Expired;
            } else if !self.polling_pairing {
                self.polling_pairing = true;
                effects.push(Effect::PairingState(info.id));
            }
        }
        effects
    }

    /// A status from the agent: the screen follows it, and the app's start-at-login entry follows the agent's.
    fn status_came(&mut self, status: Status) -> Vec<Effect> {
        let account_changed =
            self.overview().is_some_and(|overview| overview.status.account != status.account || overview.status.server != status.server);
        if !status.signed_in {
            self.usage_request_id = self.usage_request_id.wrapping_add(1);
        }
        // What the admin check said belongs to the account it was made for.
        let other_account = self.status.as_ref().is_some_and(|old| old.account != status.account || old.server != status.server);
        if !status.signed_in || other_account {
            self.access = Access::Unknown;
        }
        let new_usage_request = (status.signed_in && account_changed).then(|| self.next_usage_request());
        let mut effects = Vec::new();
        let access_now = self.access.clone();
        let mut reset_shared = false;
        self.status = Some(status.clone());
        if let Some(wanted) = app_autostart_wanted(&status, self.login_entry)
            && self.app_autostart != Some(wanted)
        {
            self.app_autostart = Some(wanted);
            effects.push(Effect::AppAutostart(wanted));
        }
        if status.removed && status.signed_in {
            self.screen = Screen::Removed;
            return effects;
        }
        if status.signed_in {
            match &mut self.screen {
                Screen::SignedIn(overview) => {
                    overview.status = status;
                    if account_changed {
                        overview.usage = None;
                        overview.usage_loading = true;
                        overview.usage_error = None;
                    }
                }
                Screen::AddingComputer(adding) => {
                    adding.overview.status = status;
                    if account_changed {
                        adding.overview.usage = None;
                        adding.overview.usage_loading = true;
                        adding.overview.usage_error = None;
                    }
                }
                Screen::Security(page) => page.overview.status = status,
                Screen::Admin(admin) => admin.overview.status = status,
                Screen::Activity(activity) => activity.overview.status = status,
                Screen::Shared(shared) => {
                    if account_changed {
                        // Another account: nothing of the old one's spaces, members or answers in flight stays.
                        let mut overview = Overview::new(status);
                        overview.access = access_now.clone();
                        **shared = SharedScreen::new(overview);
                        reset_shared = true;
                    } else {
                        shared.overview.status = status;
                    }
                }
                // The join's own answer moves on from there.
                Screen::Joining { .. } => {}
                Screen::Versions(_) | Screen::Trash(_) | Screen::Files(_) => {}
                _ => {
                    let overview = Overview::new(status);
                    let session = overview.session;
                    self.screen = Screen::SignedIn(overview);
                    effects.push(Effect::Devices);
                    // Whether the account is an admin, for the admin pages.
                    effects.push(Effect::Account { session });
                    effects.push(self.next_usage_request());
                    effects.push(Effect::Recent { session });
                    effects.extend(self.overview_mut().map(Overview::ask_conflicts));
                }
            }
            if let Some(effect) = new_usage_request {
                effects.push(effect);
            }
            if reset_shared {
                effects.extend(self.ask_spaces());
            }
            return effects;
        }
        match (&status.run_state, &self.screen) {
            (RunState::Paused { reason, .. }, _) => {
                let problem = Problem { line: first_sentence(reason), details: reason.clone(), action: ProblemAction::ForgetAccount };
                if !matches!(&self.screen, Screen::Error(p) if *p == problem) {
                    self.screen = Screen::Error(problem);
                }
            }
            (_, Screen::Join(_) | Screen::Joining { .. } | Screen::PasswordNeeded(_) | Screen::SecondFactor(_)) => {}
            _ => self.screen = Screen::Join(JoinForm::new(&self.defaults)),
        }
        effects
    }

    fn not_running(&self, line: &str, why: &str) -> Problem {
        Problem { line: line.to_owned(), details: format!("{why}\n\nIts log: {}", self.log_path), action: ProblemAction::TryAgain }
    }

    fn version_problem(&mut self, v: u32) {
        self.screen = Screen::Error(Problem {
            line: "This Craft Cloud app and its background service are different versions".into(),
            details: Error::Version(v).to_string(),
            action: ProblemAction::None,
        });
    }

    /// A status call failed: the agent went away (said at once when nothing answers, after two failed calls when
    /// something answered badly).
    fn poll_failed(&mut self, e: Error) {
        let gone = match &e {
            Error::NotRunning(_) => true,
            Error::Io(_) | Error::Protocol(_) => {
                self.failed_polls = self.failed_polls.saturating_add(1);
                self.failed_polls >= 2
            }
            Error::Version(v) => {
                self.version_problem(*v);
                return;
            }
            Error::Refused { .. } | Error::RefusedWith { .. } => false,
        };
        if !gone || matches!(self.screen, Screen::Starting | Screen::NotRunning(_)) || self.busy == Some(Busy::Quitting) {
            return;
        }
        self.status = None;
        self.screen = Screen::NotRunning(self.not_running(STOPPED, &e.to_string()));
    }

    /// Continue on the step the form shows: its fields are checked (with the messages of the whole form), then the next step
    /// is shown; the last step signs in or joins.
    fn join_next(&mut self) -> Vec<Effect> {
        let Screen::Join(form) = &mut self.screen else { return Vec::new() };
        form.error = None;
        let problem = match form.step {
            // A join string names its server itself.
            JoinStep::Server if form.mode == JoinMode::SignIn && form.server.trim().is_empty() => Some(SERVER_EMPTY.to_owned()),
            JoinStep::Account if form.mode == JoinMode::SignIn => {
                if form.user.trim().is_empty() {
                    Some(USER_EMPTY.to_owned())
                } else if form.password.is_empty() {
                    Some(SIGN_IN_PASSWORD_EMPTY.to_owned())
                } else {
                    None
                }
            }
            JoinStep::Account => JoinString::parse(&form.join).err().map(|e| sentence(&e.to_string())),
            _ => None,
        };
        if problem.is_some() {
            form.error = problem;
            return Vec::new();
        }
        match form.step.next() {
            Some(step) => {
                form.step = step;
                Vec::new()
            }
            None => self.submit_join(),
        }
    }

    fn submit_join(&mut self) -> Vec<Effect> {
        let Screen::Join(form) = &mut self.screen else { return Vec::new() };
        form.error = None;
        if form.mode == JoinMode::SignIn {
            return self.submit_sign_in();
        }
        match JoinString::parse(&form.join) {
            Ok(parsed) => form.join = parsed.join(),
            Err(e) => {
                form.error = Some(sentence(&e.to_string()));
                form.step = JoinStep::Account;
                return Vec::new();
            }
        }
        if !Path::new(form.folder.trim()).is_absolute() {
            form.error = Some(FOLDER_NOT_ABSOLUTE.to_owned());
            form.step = JoinStep::Folder;
            return Vec::new();
        }
        let request = form.request(None);
        self.screen = Screen::Joining { form: form.clone(), password: None };
        vec![Effect::Join(request)]
    }

    /// The password sign-in: the fields are checked, then the password leaves the form with the request.
    fn submit_sign_in(&mut self) -> Vec<Effect> {
        let Screen::Join(form) = &mut self.screen else { return Vec::new() };
        // The message, and the step that has the field it is about.
        let problem = if form.server.trim().is_empty() {
            Some((SERVER_EMPTY, JoinStep::Server))
        } else if form.user.trim().is_empty() {
            Some((USER_EMPTY, JoinStep::Account))
        } else if form.password.is_empty() {
            Some((SIGN_IN_PASSWORD_EMPTY, JoinStep::Account))
        } else if !Path::new(form.folder.trim()).is_absolute() {
            Some((FOLDER_NOT_ABSOLUTE, JoinStep::Folder))
        } else {
            None
        };
        if let Some((problem, step)) = problem {
            form.error = Some(problem.to_owned());
            form.step = step;
            return Vec::new();
        }
        let password = std::mem::take(&mut form.password);
        let request = form.login_request(password);
        form.challenge = None;
        self.screen = Screen::Joining { form: form.clone(), password: None };
        vec![Effect::Login(request)]
    }

    /// The code of a join that asked for one: the same join, with the code, goes to the agent again.
    fn submit_second_factor(&mut self) -> Vec<Effect> {
        let Screen::SecondFactor(second) = &mut self.screen else { return Vec::new() };
        let factor = match second.factor() {
            Ok(factor) => factor,
            Err(why) => {
                second.error = Some(why);
                return Vec::new();
            }
        };
        if second.form.mode == JoinMode::SignIn {
            // The password is not asked for again: the challenge stands for it.
            let Some(request) = second.form.second_step_request(factor) else {
                second.error = Some(NO_CHALLENGE.to_owned());
                return Vec::new();
            };
            let form = second.form.clone();
            self.screen = Screen::Joining { form, password: None };
            return vec![Effect::LoginSecondFactor(request)];
        }
        let request = second.form.request_with_factor(second.password.as_ref().map(|pw| pw.password.clone()), factor);
        let (form, password) = (second.form.clone(), second.password.clone());
        self.screen = Screen::Joining { form, password };
        vec![Effect::Join(request)]
    }

    /// A button of the Security page.
    fn security(&mut self, event: SecurityEvent) -> Vec<Effect> {
        let Screen::Security(page) = &mut self.screen else { return Vec::new() };
        if event == SecurityEvent::Close {
            // Whatever the page showed (a secret, recovery codes) goes with it.
            let overview = page.overview.restored();
            self.screen = Screen::SignedIn(overview);
            return vec![Effect::Devices];
        }
        page.update(event).into_iter().map(Effect::Security).collect()
    }

    fn submit_password(&mut self) -> Vec<Effect> {
        let Screen::PasswordNeeded(pw) = &mut self.screen else { return Vec::new() };
        pw.error = if pw.password.is_empty() {
            Some(PASSWORD_EMPTY.to_owned())
        } else if pw.password != pw.again {
            Some(PASSWORDS_DIFFER.to_owned())
        } else {
            None
        };
        if pw.error.is_some() {
            return Vec::new();
        }
        let request = pw.form.request(Some(pw.password.clone()));
        self.screen = Screen::Joining { form: pw.form.clone(), password: Some(pw.clone()) };
        vec![Effect::Join(request)]
    }

    fn joined(&mut self, result: Result<Status, Error>) -> Vec<Effect> {
        let Screen::Joining { form, password } = &self.screen else { return Vec::new() };
        let (mut form, password) = (form.clone(), password.clone());
        let signing_in = form.mode == JoinMode::SignIn;
        // The challenge is kept only for a second step that is still asked for.
        let challenge = form.challenge.take();
        let e = match result {
            Ok(status) => {
                self.screen = Screen::SignedIn(Overview::new(status.clone()));
                let mut effects = self.status_came(status);
                // Start at login: the agent's entry (the app's follows it).
                effects.extend([Effect::SetAutostart(true), Effect::Devices]);
                effects.extend(self.overview().map(|o| Effect::Account { session: o.session }));
                effects.extend(self.overview().map(|o| Effect::Recent { session: o.session }));
                effects.push(self.next_usage_request());
                return effects;
            }
            Err(e) => e,
        };
        let (code, text) = match &e {
            Error::Refused { code, message, .. } | Error::RefusedWith { code, message, .. } => (code.as_str(), message.clone()),
            Error::NotRunning(_) => {
                self.status = None;
                self.screen = Screen::NotRunning(self.not_running(STOPPED, &e.to_string()));
                return Vec::new();
            }
            other => ("", other.to_string()),
        };
        let back_to_password = |mut pw: PasswordForm, error: String| {
            pw.password.clear();
            pw.again.clear();
            pw.error = Some(error);
            Screen::PasswordNeeded(pw)
        };
        self.screen = match (code, password) {
            // A sign-in's password was right and the account has two-factor sign-in: the code goes with the challenge.
            (codes::TWO_FACTOR_REQUIRED, _) if signing_in => match e.challenge() {
                Some(c) => {
                    form.challenge = Some(c.challenge.clone());
                    form.challenge_expires_ms = c.expires_ms;
                    Screen::SecondFactor(SecondFactorForm::new(form, None, None))
                }
                None => {
                    form.error = Some(NO_CHALLENGE.to_owned());
                    Screen::Join(form)
                }
            },
            (codes::TWO_FACTOR_EXPIRED, _) if signing_in => {
                form.error = Some(CHALLENGE_EXPIRED.to_owned());
                Screen::Join(form)
            }
            // The account has two-factor sign-in: the same join goes again with its code (the invite was not used up).
            (codes::TWO_FACTOR_REQUIRED, password) => Screen::SecondFactor(SecondFactorForm::new(form, password, None)),
            // A lock outlasts the challenge: a sign-in starts over then, and says how long to wait.
            (codes::TWO_FACTOR_LOCKED, _) if signing_in && !wait_fits(e.retry_after_secs(), form.challenge_expires_ms, self.now_ms) => {
                form.error = Some(crate::security::explain(&e));
                Screen::Join(form)
            }
            (codes::TWO_FACTOR_INVALID | codes::TWO_FACTOR_LOCKED, password) => {
                form.challenge = challenge;
                Screen::SecondFactor(SecondFactorForm::new(form, password, Some(crate::security::explain(&e))))
            }
            (codes::PASSWORD_REQUIRED, _) => {
                Screen::PasswordNeeded(PasswordForm { form, password: String::new(), again: String::new(), error: None })
            }
            (codes::WEAK_PASSWORD, Some(pw)) => back_to_password(pw, text),
            (codes::FOLDER_HAS_FILES, _) => {
                form.ask_adopt = true;
                Screen::Join(form)
            }
            (codes::INVALID_CODE, _) => {
                form.error = Some(INVALID_CODE.to_owned());
                Screen::Join(form)
            }
            // Signed in meanwhile: the form, until the status (asked now) shows the signed-in view.
            (codes::ALREADY_SIGNED_IN, _) => {
                self.screen = Screen::Join(form);
                return vec![Effect::Status];
            }
            (_, Some(pw)) => back_to_password(pw, text),
            // The agent says `offline` for anything that kept the server from answering, a busy server (429) included, and
            // its message says which: a sign-in shows it. A second step goes on with its challenge while that lasts.
            (codes::OFFLINE, None) if signing_in => {
                let said = if text.trim().is_empty() { crate::security::explain(&e) } else { text };
                if challenge.is_some() && form.challenge_expires_ms > self.now_ms {
                    form.challenge = challenge;
                    Screen::SecondFactor(SecondFactorForm::new(form, None, Some(said)))
                } else {
                    form.error = Some(said);
                    Screen::Join(form)
                }
            }
            (_, None) => {
                form.error = Some(if signing_in { crate::security::explain(&e) } else { text });
                Screen::Join(form)
            }
        };
        // What the problem is about is on the account step: a sign-in's password has to be typed again (it is not kept), and
        // a join string that doesn't work is changed there. For the rest (the folder has files) the step stays.
        if let Screen::Join(form) = &mut self.screen
            && form.error.is_some()
            && !form.ask_adopt
            && form.step == JoinStep::Folder
            && (form.mode == JoinMode::SignIn || form.error.as_deref() == Some(INVALID_CODE))
        {
            form.step = JoinStep::Account;
        }
        Vec::new()
    }

    fn pairing_polled(&mut self, result: Result<PairingStatus, Error>) -> Vec<Effect> {
        self.polling_pairing = false;
        let Screen::AddingComputer(adding) = &mut self.screen else { return Vec::new() };
        if adding.state != AddState::Waiting {
            return Vec::new();
        }
        match result {
            Ok(PairingStatus { state: PairingState::Used, device_name }) => {
                adding.state = AddState::Joined(device_name.unwrap_or_else(|| "A computer".into()));
                vec![Effect::Devices]
            }
            Ok(PairingStatus { state: PairingState::Expired, .. }) => {
                adding.state = AddState::Expired;
                Vec::new()
            }
            Err(Error::Refused { code, .. }) if code == api_codes::NOT_FOUND => {
                adding.state = AddState::Expired;
                Vec::new()
            }
            // Still waiting, or a call that failed: asked again at the next tick.
            Ok(_) | Err(_) => Vec::new(),
        }
    }

    fn sign_out(&mut self, local: bool) -> Vec<Effect> {
        // Signing out drops the page as a tab would: not while it holds a one-time code or waits for a call.
        if self.busy.is_some() || self.tabs_locked() {
            return Vec::new();
        }
        // From every page of the signed-in window, the footer's Sign out included.
        if self.tab().is_none() {
            return Vec::new();
        }
        self.sign_out_error = None;
        self.offer_local_sign_out = false;
        self.busy = Some(Busy::SigningOut);
        vec![Effect::Logout { local }]
    }

    fn request_history(&mut self, cursor: Option<String>) -> Vec<Effect> {
        let Screen::Versions(versions) = &mut self.screen else { return Vec::new() };
        if versions.restoring || versions.loading {
            return Vec::new();
        }
        // The path is sent exactly as typed: a leading or trailing space can be part of a real file name.
        let path = versions.path.as_str();
        if path.is_empty()
            || Path::new(path).is_absolute()
            || Path::new(path)
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir | std::path::Component::RootDir | std::path::Component::Prefix(_)))
        {
            versions.error = Some("Enter a file path inside the sync folder.".into());
            return Vec::new();
        }
        self.versions_ticket = self.versions_ticket.saturating_add(1);
        versions.ticket = self.versions_ticket;
        versions.loading = true;
        versions.error = None;
        vec![Effect::History { path: path.to_owned(), cursor, ticket: versions.ticket }]
    }

    fn request_files(&mut self) -> Vec<Effect> {
        let root = self.status.as_ref().and_then(|s| s.folder.clone());
        let Screen::Files(files) = &mut self.screen else { return Vec::new() };
        if files.loading {
            return Vec::new();
        }
        self.files_ticket = self.files_ticket.saturating_add(1);
        files.ticket = self.files_ticket;
        files.loading = true;
        files.error = None;
        files.open_error = None;
        files.conflicts.ticket = crate::admin::fresh();
        files.conflicts.loading = true;
        files.conflicts.error = None;
        files.conflicts.open_error = None;
        vec![Effect::Recents { root: root.clone(), ticket: files.ticket }, Effect::Conflicts { root, ticket: files.conflicts.ticket }]
    }

    /// Opens a conflict copy with the OS's default program, or (`original`) the folder of the file it is a copy of, if that is
    /// in the synced folder here: the path is joined to the folder and checked again however the list placed it. A copy is
    /// checked as a recent file is ([`crate::files::resolve`], then on the disk as it is at this moment by
    /// [`Effect::CheckFile`], and what that checks is opened). The original is shown by its folder, whatever kind of file it is
    /// (so it may be one that isn't opened from the page), and the folder gets only the checks of a path
    /// ([`crate::files::resolve_folder`]: no escape, no bundle). Otherwise the page says why not.
    fn open_conflict(&mut self, path: &str, original: bool) -> Vec<Effect> {
        let root = self.status.as_ref().and_then(|s| s.folder.clone());
        let Screen::Files(files) = &mut self.screen else { return Vec::new() };
        let Some(file) = files.conflicts.list.iter().flat_map(|list| list.files.iter()).find(|f| f.copy.path == path) else { return Vec::new() };
        let (place, target, what) = match (original, &file.copy.original_path, file.original) {
            (false, _, _) => (file.place, file.copy.path.clone(), "open this file"),
            (true, Some(original_path), Some(place)) => (place, original_path.clone(), "show the original"),
            (true, ..) => {
                files.conflicts.open_error = Some("This copy's name doesn't say which file it is a copy of.".into());
                return Vec::new();
            }
        };
        files.conflicts.open_error = None;
        let outcome = match (original, place, root) {
            (true, Place::Here | Place::NotOpenable, Some(root)) => match target.rsplit_once('/') {
                // The folder the original is in; the synced folder itself for one at the top.
                Some((parent, _)) => crate::files::resolve_folder(Path::new(&root), parent).map(|full| full.to_string_lossy().into_owned()),
                None => Ok(root),
            }
            .map(Effect::OpenFolder),
            (false, Place::Here, Some(root)) => crate::files::resolve(Path::new(&root), &target).map(|_| Effect::CheckFile { root, path: target }),
            (true, Place::Here | Place::NotOpenable, None) | (false, Place::Here, None) => Err(Place::NotYet),
            (_, place, _) => Err(place),
        };
        match outcome {
            Ok(effect) => vec![effect],
            Err(place) => {
                files.conflicts.open_error = place.reason().map(|why| format!("Can't {what}: {why}."));
                Vec::new()
            }
        }
    }

    /// Opens a recent file with the OS's default program, if it is in the synced folder here: the path is checked again by
    /// its text ([`crate::files::resolve`]) however the list placed it, then on the disk as it is at this moment
    /// ([`Effect::CheckFile`]: the list may be old, and a file may have been swapped for a link since), and the path that
    /// check gives is the one opened. Otherwise the page says why not.
    fn open_recent(&mut self, app: &str, path: &str) -> Vec<Effect> {
        let root = self.status.as_ref().and_then(|s| s.folder.clone());
        let Screen::Files(files) = &mut self.screen else { return Vec::new() };
        let Some(file) = files.list.iter().flatten().find(|f| f.app == app && f.entry.path == path) else { return Vec::new() };
        let place = file.place;
        let path = file.entry.path.clone();
        files.open_error = None;
        let outcome = match (place, root) {
            (Place::Here, Some(root)) => crate::files::resolve(Path::new(&root), &path).map(|_| root),
            (Place::Here, None) => Err(Place::NotYet),
            (place, _) => Err(place),
        };
        match outcome {
            Ok(root) => vec![Effect::CheckFile { root, path }],
            Err(place) => {
                files.open_error = place.reason().map(|why| format!("Can't open this file: {why}."));
                Vec::new()
            }
        }
    }

    /// Shows a recent file that is here in the file manager, by opening the folder it is in (the synced folder for a file at
    /// its top). A file that can't be opened can be shown; one that isn't here, or whose folder is a bundle, can't.
    fn show_recent(&mut self, app: &str, path: &str) -> Vec<Effect> {
        let root = self.status.as_ref().and_then(|s| s.folder.clone());
        let Screen::Files(files) = &mut self.screen else { return Vec::new() };
        let Some(file) = files.list.iter().flatten().find(|f| f.app == app && f.entry.path == path) else { return Vec::new() };
        files.open_error = None;
        let (place, path) = (file.place, file.entry.path.clone());
        let folder = match (place, root) {
            (Place::Here | Place::NotOpenable, Some(root)) => match path.rsplit_once('/') {
                Some((parent, _)) => crate::files::resolve_folder(Path::new(&root), parent),
                None => match crate::files::resolve_folder(Path::new(&root), &path) {
                    Err(Place::Outside) => Err(Place::Outside),
                    _ => Ok(PathBuf::from(&root)),
                },
            },
            (Place::Here | Place::NotOpenable, None) => Err(Place::NotYet),
            (place, _) => Err(place),
        };
        match folder {
            Ok(folder) => vec![Effect::OpenFolder(folder.to_string_lossy().into_owned())],
            Err(place) => {
                files.open_error = place.reason().map(|why| format!("Can't show this file: {why}."));
                Vec::new()
            }
        }
    }

    fn request_trash(&mut self, cursor: Option<String>) -> Vec<Effect> {
        let Screen::Trash(trash) = &mut self.screen else { return Vec::new() };
        if trash.loading || trash.action.is_some() {
            return Vec::new();
        }
        self.trash_ticket = self.trash_ticket.saturating_add(1);
        trash.ticket = self.trash_ticket;
        trash.loading = true;
        trash.error = None;
        vec![Effect::Trash { cursor, ticket: trash.ticket }]
    }

    fn begin_trash_action(&mut self, id: NodeId, action: TrashAction) -> Vec<Effect> {
        let Screen::Trash(trash) = &mut self.screen else { return Vec::new() };
        if trash.loading || trash.action.is_some() {
            return Vec::new();
        }
        self.trash_ticket = self.trash_ticket.saturating_add(1);
        trash.ticket = self.trash_ticket;
        trash.error = None;
        trash.action = Some(action);
        match &trash.action {
            Some(TrashAction::Restore { .. }) => vec![Effect::RestoreTrash { id, ticket: trash.ticket }],
            Some(TrashAction::Remove) => vec![Effect::RemoveTrash { id, ticket: trash.ticket }],
            None => Vec::new(),
        }
    }

    fn restore_version(&mut self) -> Vec<Effect> {
        let Screen::Versions(versions) = &mut self.screen else { return Vec::new() };
        // A restore takes a new ticket, which would discard the answer of a page still loading and leave `loading` set.
        if versions.restoring || versions.loading {
            return Vec::new();
        }
        let Some(version) = versions.confirm else { return Vec::new() };
        let Some(expected_version) = versions.current_version else {
            versions.error = Some("Current version unavailable. Reload the version list.".into());
            return Vec::new();
        };
        self.versions_ticket = self.versions_ticket.saturating_add(1);
        versions.ticket = self.versions_ticket;
        versions.restoring = true;
        vec![Effect::Restore { path: versions.path.clone(), version, expected_version, ticket: versions.ticket }]
    }

    fn signed_out(&mut self, result: Result<Status, Error>) -> Vec<Effect> {
        self.busy = self.busy.filter(|b| *b != Busy::SigningOut);
        match result {
            Ok(status) => {
                self.screen = Screen::Join(JoinForm::new(&self.defaults));
                let mut effects = self.status_came(status);
                // Signed out, the app no longer starts at login either.
                if self.app_autostart != Some(false) {
                    self.app_autostart = Some(false);
                    effects.retain(|e| !matches!(e, Effect::AppAutostart(_)));
                    effects.push(Effect::AppAutostart(false));
                }
                effects
            }
            Err(e) => {
                let offline = matches!(&e, Error::Refused { code, .. } if code == codes::OFFLINE);
                let text = message(&e);
                if let Screen::Error(problem) = &mut self.screen {
                    problem.details = format!("{}\n\nSigning out failed: {text}", problem.details);
                } else if self.tab().is_some() {
                    self.sign_out_error = Some(text);
                    self.offer_local_sign_out = offline;
                }
                Vec::new()
            }
        }
    }
}

/// An error's message, for people.
fn message(e: &Error) -> String {
    match e {
        Error::Refused { message, .. } | Error::RefusedWith { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// `text` starting with a capital letter (the agent's and the parser's messages start in lower case).
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// The first sentence of `text`, for a problem's one line.
pub fn first_sentence(text: &str) -> String {
    let end = text.find(". ").map_or(text.len(), |at| at + 1);
    sentence(text.get(..end).unwrap_or(text).trim())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn defaults() -> JoinDefaults {
        JoinDefaults {
            folder: if cfg!(windows) { r"C:\Users\ana\Craft Cloud".into() } else { "/home/ana/Craft Cloud".into() },
            device_name: "lptp1".into(),
        }
    }

    fn model() -> Model {
        Model::new(defaults(), true, "/home/ana/.local/share/craft-cloud-agent/run.log".into())
    }

    fn history_page() -> HistoryResponse {
        HistoryResponse {
            versions: vec![craft_cloud_app::HistoryVersion {
                id: VersionId::generate(),
                parent_version: None,
                epoch: craft_cloud_proto::Epoch::generate(),
                seq: 3,
                size: 12,
                mtime_ms: 0,
                device: craft_cloud_app::DeviceId::generate(),
                created_ms: 0,
                save: false,
                pruned: false,
            }],
            next: Some("opaque-cursor".into()),
        }
    }

    fn trash_page() -> TrashResponse {
        TrashResponse {
            items: vec![craft_cloud_app::TrashItem {
                id: NodeId::generate(),
                parent: NodeId::generate(),
                name: "painting.png".into(),
                is_dir: false,
                size: Some(42_000),
                deleted_ms: 1_790_000_000_000,
                purge_after_ms: 1_792_000_000_000,
                path: Some("Projects/painting.png".into()),
            }],
            next: Some("trash-cursor".into()),
        }
    }

    pub(crate) fn status(signed_in: bool) -> Status {
        Status {
            api: 1,
            signed_in,
            account: signed_in.then(|| "fabio".into()),
            server: signed_in.then(|| "https://cloud.shadows.ch".into()),
            folder: signed_in.then(|| "/home/ana/Craft Cloud".into()),
            machine: signed_in.then(|| "lptp1".into()),
            online: signed_in,
            run_state: if signed_in { RunState::Idle } else { RunState::SignedOut },
            autostart: Some(signed_in),
            last_sync_ms: None,
            removed: false,
            spaces: Vec::new(),
            from_folder: None,
        }
    }

    #[test]
    fn versions_load_page_and_restore_with_ticket_guard() {
        let mut m = model();
        m.screen = Screen::SignedIn(Overview::new(status(true)));
        m.update(Event::OpenVersions);
        m.update(Event::VersionsPath("art/file.png".into()));
        assert_eq!(m.update(Event::LoadVersions), vec![Effect::History { path: "art/file.png".into(), cursor: None, ticket: 2 }]);
        assert!(m.update(Event::History { ticket: 1, result: Ok(history_page()) }).is_empty());
        let mut page = history_page();
        let current = page.versions[0].id;
        let old = VersionId::generate();
        page.versions.push(craft_cloud_app::HistoryVersion {
            id: old,
            parent_version: Some(current),
            epoch: craft_cloud_proto::Epoch::generate(),
            seq: 2,
            size: 10,
            mtime_ms: 0,
            device: craft_cloud_app::DeviceId::generate(),
            created_ms: 0,
            save: false,
            pruned: false,
        });
        assert!(m.update(Event::History { ticket: 2, result: Ok(page.clone()) }).is_empty());
        assert_eq!(
            m.update(Event::NextVersions),
            vec![Effect::History { path: "art/file.png".into(), cursor: Some("opaque-cursor".into()), ticket: 3 }]
        );
        m.update(Event::History { ticket: 3, result: Ok(page.clone()) });
        m.update(Event::ConfirmRestore(old));
        assert_eq!(
            m.update(Event::RestoreVersion),
            vec![Effect::Restore { path: "art/file.png".into(), version: old, expected_version: current, ticket: 4 }]
        );
        assert!(m.update(Event::LoadVersions).is_empty());
        assert!(m.update(Event::NextVersions).is_empty());
        assert!(
            m.update(Event::Restored {
                ticket: 3,
                result: Ok(craft_cloud_app::RestoreResponse {
                    version: old,
                    parent_version: current,
                    seq: 5,
                    epoch: craft_cloud_proto::Epoch::generate()
                })
            })
            .is_empty()
        );
        assert!(
            m.update(Event::Restored {
                ticket: 4,
                result: Err(Error::Refused { code: api_codes::CONFLICT.into(), message: "conflict".into(), name_taken: None })
            })
            .iter()
            .any(|e| matches!(e, Effect::History { cursor: None, .. }))
        );
        let Screen::Versions(v) = &m.screen else { panic!("expected versions screen") };
        assert_eq!(v.notice.as_deref(), Some("The file changed meanwhile. Version list reloaded."));
    }

    #[test]
    fn trash_pages_restore_remove_confirmation_and_stale_tickets() {
        let mut m = model();
        m.screen = Screen::SignedIn(Overview::new(status(true)));
        assert_eq!(m.update(Event::OpenTrash), vec![Effect::Trash { cursor: None, ticket: 1 }]);
        let page = trash_page();
        let id = page.items[0].id;
        assert!(m.update(Event::TrashPage { ticket: 0, result: Ok(page.clone()) }).is_empty());
        assert!(matches!(&m.screen, Screen::Trash(t) if t.loading && t.page.is_none()));
        m.update(Event::TrashPage { ticket: 1, result: Ok(page.clone()) });
        assert_eq!(m.update(Event::NextTrash), vec![Effect::Trash { cursor: Some("trash-cursor".into()), ticket: 2 }]);
        assert!(m.update(Event::RestoreTrash(id)).is_empty()); // page load excludes actions
        assert!(m.update(Event::ConfirmRemoveTrash(id)).is_empty());
        assert!(m.update(Event::RemoveTrash).is_empty());
        m.update(Event::TrashPage { ticket: 2, result: Ok(page.clone()) });
        assert_eq!(m.update(Event::RestoreTrash(id)), vec![Effect::RestoreTrash { id, ticket: 3 }]);
        assert!(m.update(Event::LoadTrash).is_empty()); // action excludes page loads
        assert!(m.update(Event::TrashRestored { ticket: 2, result: Err(Error::Io("stale".into())) }).is_empty());
        assert!(matches!(&m.screen, Screen::Trash(t) if t.action.is_some() && !t.loading));
        let restored = TrashRestoreResponse {
            node: id,
            parent: page.items[0].parent,
            name: "painting (1).png".into(),
            renamed: true,
            version: None,
            seq: 2,
            epoch: craft_cloud_proto::Epoch::generate(),
        };
        assert_eq!(m.update(Event::TrashRestored { ticket: 3, result: Ok(restored) }), vec![Effect::Trash { cursor: None, ticket: 4 }]);
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert_eq!(t.notice.as_deref(), Some("Restored as painting (1).png in Projects."));
        m.update(Event::TrashPage { ticket: 4, result: Ok(page.clone()) });
        assert!(m.update(Event::ConfirmRemoveTrash(id)).is_empty());
        assert!(matches!(&m.screen, Screen::Trash(t) if t.confirm == Some(id)));
        assert_eq!(m.update(Event::RemoveTrash), vec![Effect::RemoveTrash { id, ticket: 5 }]);
        assert!(m.update(Event::LoadTrash).is_empty()); // removal excludes page loads too
        assert!(m.update(Event::TrashRemoved { ticket: 4, result: Ok(()) }).is_empty());
        assert_eq!(m.update(Event::TrashRemoved { ticket: 5, result: Ok(()) }), vec![Effect::Trash { cursor: None, ticket: 6 }]);
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert_eq!(t.notice.as_deref(), Some("Removed from trash. It can no longer be restored from the trash."));
    }

    #[test]
    fn trash_restore_name_taken_is_clear_and_returns_to_idle_without_reload() {
        let mut m = model();
        let page = trash_page();
        let id = page.items[0].id;
        m.screen = Screen::Trash(Trash { page: Some(page), ticket: 0, ..Trash::new() });
        assert!(m.update(Event::RemoveTrash).is_empty());
        assert!(matches!(m.update(Event::RestoreTrash(id)).first(), Some(Effect::RestoreTrash { .. })));
        let ticket = match &m.screen {
            Screen::Trash(t) => t.ticket,
            _ => 0,
        };
        assert!(
            m.update(Event::TrashRestored {
                ticket,
                result: Err(Error::Refused { code: api_codes::CONFLICT.into(), message: "conflict".into(), name_taken: Some(true) })
            })
            .is_empty()
        );
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert!(t.error.as_deref().unwrap_or_default().contains("another item already has that name"));
        assert!(!t.loading);
        assert!(t.action.is_none());
    }

    #[test]
    fn trash_restore_without_path_does_not_invent_a_destination() {
        let mut m = model();
        let mut page = trash_page();
        page.items[0].path = None;
        let id = page.items[0].id;
        m.screen = Screen::Trash(Trash { page: Some(page), ticket: 0, ..Trash::new() });
        assert!(matches!(m.update(Event::RestoreTrash(id)).first(), Some(Effect::RestoreTrash { .. })));
        let ticket = match &m.screen {
            Screen::Trash(t) => t.ticket,
            _ => 0,
        };
        assert_eq!(
            m.update(Event::TrashRestored {
                ticket,
                result: Ok(TrashRestoreResponse {
                    node: id,
                    parent: NodeId::generate(),
                    name: "painting.png".into(),
                    renamed: false,
                    version: None,
                    seq: 1,
                    epoch: craft_cloud_proto::Epoch::generate(),
                })
            }),
            vec![Effect::Trash { cursor: None, ticket: ticket + 1 }]
        );
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert_eq!(t.notice.as_deref(), Some("Restored painting.png. The destination path is unavailable."));
    }

    #[test]
    fn other_trash_conflicts_are_generic_and_do_not_reload() {
        let mut m = model();
        let page = trash_page();
        let id = page.items[0].id;
        m.screen = Screen::Trash(Trash { page: Some(page), ticket: 0, ..Trash::new() });
        assert!(matches!(m.update(Event::RestoreTrash(id)).first(), Some(Effect::RestoreTrash { .. })));
        let ticket = match &m.screen {
            Screen::Trash(t) => t.ticket,
            _ => 0,
        };
        assert!(
            m.update(Event::TrashRestored {
                ticket,
                result: Err(Error::Refused { code: api_codes::CONFLICT.into(), message: "conflict".into(), name_taken: Some(false) })
            })
            .is_empty()
        );
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert!(t.error.as_deref().unwrap_or_default().contains("conflicts with the current folder contents"));
        assert!(!t.loading);
        assert!(t.action.is_none());
    }

    fn trash_restore_response(node: NodeId, name: &str, renamed: bool) -> TrashRestoreResponse {
        TrashRestoreResponse {
            node,
            parent: NodeId::generate(),
            name: name.into(),
            renamed,
            version: None,
            seq: 1,
            epoch: craft_cloud_proto::Epoch::generate(),
        }
    }

    fn trash_ticket(m: &Model) -> u64 {
        match &m.screen {
            Screen::Trash(t) => t.ticket,
            _ => 0,
        }
    }

    #[test]
    fn trash_restore_at_the_sync_root_says_so_even_when_renamed() {
        let mut m = model();
        let mut page = trash_page();
        page.items[0].path = Some("painting.png".into());
        let id = page.items[0].id;
        m.screen = Screen::Trash(Trash { page: Some(page), ticket: 0, ..Trash::new() });
        assert!(matches!(m.update(Event::RestoreTrash(id)).first(), Some(Effect::RestoreTrash { .. })));
        let ticket = trash_ticket(&m);
        m.update(Event::TrashRestored { ticket, result: Ok(trash_restore_response(id, "painting (1).png", true)) });
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert_eq!(t.notice.as_deref(), Some("Restored as painting (1).png in the sync folder."));
    }

    #[test]
    fn trash_restore_renamed_without_path_keeps_the_unavailable_notice() {
        let mut m = model();
        let mut page = trash_page();
        page.items[0].path = None;
        let id = page.items[0].id;
        m.screen = Screen::Trash(Trash { page: Some(page), ticket: 0, ..Trash::new() });
        m.update(Event::RestoreTrash(id));
        let ticket = trash_ticket(&m);
        m.update(Event::TrashRestored { ticket, result: Ok(trash_restore_response(id, "painting (1).png", true)) });
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert_eq!(t.notice.as_deref(), Some("Restored as painting (1).png. The destination path is unavailable."));
    }

    #[test]
    fn back_from_trash_or_versions_asks_again_for_usage_and_admin_access() {
        for (open, back) in [(Event::OpenTrash, Event::BackTrash), (Event::OpenVersions, Event::BackVersions)] {
            let mut m = model();
            m.status = Some(status(true));
            m.screen = Screen::SignedIn(Overview::new(status(true)));
            m.update(open);
            let effects = m.update(back);
            let overview = m.overview().expect("back on the overview");
            assert!(overview.usage_loading, "{effects:?}");
            let session = overview.session;
            assert!(effects.contains(&Effect::Devices), "{effects:?}");
            assert!(effects.contains(&Effect::Account { session }), "{effects:?}");
            let Some(&Effect::Usage(id)) = effects.iter().find(|e| matches!(e, Effect::Usage(_))) else { panic!("{effects:?}") };
            let usage = craft_cloud_app::AccountUsage {
                stored_bytes: 1,
                quota_bytes: None,
                trash_bytes: None,
                history_bytes: None,
                estimates: craft_cloud_app::UsageEstimates { index_complete: true, reason: None },
            };
            m.update(Event::Usage { request_id: id, result: Ok(usage) });
            let overview = m.overview().unwrap();
            assert!(!overview.usage_loading && overview.usage.is_some(), "the overview doesn't stay loading");
        }
    }

    #[test]
    fn trash_late_page_reply_after_back_and_reopen_is_dropped() {
        let mut m = model();
        m.status = Some(status(true));
        m.screen = Screen::SignedIn(Overview::new(status(true)));
        assert_eq!(m.update(Event::OpenTrash), vec![Effect::Trash { cursor: None, ticket: 1 }]);
        m.update(Event::BackTrash);
        assert!(matches!(m.screen, Screen::SignedIn(_)));
        let reopened = m.update(Event::OpenTrash);
        let Some(Effect::Trash { ticket: fresh, .. }) = reopened.first().cloned() else { panic!("{reopened:?}") };
        assert_ne!(fresh, 1);
        assert!(m.update(Event::TrashPage { ticket: 1, result: Ok(trash_page()) }).is_empty());
        assert!(matches!(&m.screen, Screen::Trash(t) if t.loading && t.page.is_none()));
        m.update(Event::TrashPage { ticket: fresh, result: Ok(trash_page()) });
        assert!(matches!(&m.screen, Screen::Trash(t) if !t.loading && t.page.is_some() && t.error.is_none()));
    }

    #[test]
    fn trash_late_action_reply_after_back_and_reopen_is_dropped() {
        let mut m = model();
        let page = trash_page();
        let id = page.items[0].id;
        m.status = Some(status(true));
        m.screen = Screen::SignedIn(Overview::new(status(true)));
        m.update(Event::OpenTrash);
        m.update(Event::TrashPage { ticket: 1, result: Ok(page.clone()) });
        assert_eq!(m.update(Event::RestoreTrash(id)), vec![Effect::RestoreTrash { id, ticket: 2 }]);
        m.update(Event::BackTrash);
        let reopened = m.update(Event::OpenTrash);
        let Some(Effect::Trash { ticket: fresh, .. }) = reopened.first().cloned() else { panic!("{reopened:?}") };
        assert!(m.update(Event::TrashRestored { ticket: 2, result: Ok(trash_restore_response(id, "painting.png", false)) }).is_empty());
        assert!(m.update(Event::TrashRemoved { ticket: 2, result: Ok(()) }).is_empty());
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert!(t.loading && t.notice.is_none());
        m.update(Event::TrashPage { ticket: fresh, result: Ok(page) });
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert!(!t.loading && t.action.is_none() && t.page.is_some() && t.error.is_none());
        assert!(matches!(m.update(Event::RestoreTrash(id)).first(), Some(Effect::RestoreTrash { .. })));
    }

    #[test]
    fn trash_remove_failure_shows_the_error_and_frees_the_buttons() {
        let mut m = model();
        let page = trash_page();
        let id = page.items[0].id;
        m.screen = Screen::Trash(Trash { page: Some(page), ticket: 0, ..Trash::new() });
        m.update(Event::ConfirmRemoveTrash(id));
        assert!(matches!(m.update(Event::RemoveTrash).first(), Some(Effect::RemoveTrash { .. })));
        let ticket = trash_ticket(&m);
        assert!(
            m.update(Event::TrashRemoved {
                ticket,
                result: Err(Error::Refused { code: api_codes::INTERNAL.into(), message: "server broke".into(), name_taken: None })
            })
            .is_empty()
        );
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert!(t.error.is_some());
        assert!(t.action.is_none() && !t.loading && t.confirm.is_none());
        assert!(t.page.is_some());
        assert!(matches!(m.update(Event::RestoreTrash(id)).first(), Some(Effect::RestoreTrash { .. })));
    }

    #[test]
    fn trash_failed_next_page_keeps_rows_clears_loading_and_can_retry() {
        let mut m = model();
        m.screen = Screen::SignedIn(Overview::new(status(true)));
        m.update(Event::OpenTrash);
        let page = trash_page();
        m.update(Event::TrashPage { ticket: 1, result: Ok(page.clone()) });
        assert_eq!(m.update(Event::NextTrash), vec![Effect::Trash { cursor: Some("trash-cursor".into()), ticket: 2 }]);
        assert!(m.update(Event::TrashPage { ticket: 2, result: Err(Error::Io("offline".into())) }).is_empty());
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert!(t.error.is_some());
        assert!(!t.loading);
        assert_eq!(t.page.as_ref(), Some(&page));
        assert_eq!(m.update(Event::NextTrash), vec![Effect::Trash { cursor: Some("trash-cursor".into()), ticket: 3 }]);
        let Screen::Trash(t) = &m.screen else { panic!("expected trash screen") };
        assert!(t.loading && t.error.is_none());
    }

    #[test]
    fn versions_refuse_unsafe_paths_and_explain_invalid_restore() {
        let mut m = model();
        m.screen = Screen::Versions(Versions {
            path: "../secret".into(),
            page: None,
            current_version: None,
            loading: false,
            restoring: false,
            error: None,
            notice: None,
            confirm: None,
            ticket: 0,
        });
        assert!(m.update(Event::LoadVersions).is_empty());
        let Screen::Versions(v) = &m.screen else { panic!("expected versions screen") };
        assert!(v.error.as_deref().unwrap_or_default().contains("inside the sync folder"));
        let mut page = history_page();
        let current_version = page.versions.first().map(|version| version.id);
        let old_version = VersionId::generate();
        if let Some(current) = current_version {
            page.versions.push(craft_cloud_app::HistoryVersion {
                id: old_version,
                parent_version: Some(current),
                epoch: craft_cloud_proto::Epoch::generate(),
                seq: 2,
                size: 10,
                mtime_ms: 0,
                device: craft_cloud_app::DeviceId::generate(),
                created_ms: 0,
                save: false,
                pruned: false,
            });
        }
        m.screen = Screen::Versions(Versions {
            path: "file.txt".into(),
            page: Some(page),
            current_version,
            loading: false,
            restoring: false,
            error: None,
            notice: None,
            confirm: Some(old_version),
            ticket: 0,
        });
        let effect = m.update(Event::RestoreVersion);
        let ticket = match effect.first() {
            Some(Effect::Restore { ticket, .. }) => *ticket,
            other => panic!("{other:?}"),
        };
        m.update(Event::Restored {
            ticket,
            result: Err(Error::Refused { code: api_codes::INVALID.into(), message: "invalid".into(), name_taken: None }),
        });
        let Screen::Versions(v) = &m.screen else { panic!("expected versions screen") };
        assert!(v.error.as_deref().unwrap_or_default().contains("live document or deleted"));
    }

    #[test]
    fn versions_path_change_discards_answers_for_previous_path() {
        let mut m = model();
        m.screen = Screen::Versions(Versions {
            path: "old.txt".into(),
            page: None,
            current_version: None,
            loading: false,
            restoring: false,
            error: None,
            notice: None,
            confirm: None,
            ticket: 0,
        });
        assert_eq!(m.update(Event::LoadVersions), vec![Effect::History { path: "old.txt".into(), cursor: None, ticket: 1 }]);
        m.update(Event::VersionsPath("new.txt".into()));
        assert!(m.update(Event::History { ticket: 1, result: Ok(history_page()) }).is_empty());
        let Screen::Versions(v) = &m.screen else { panic!("expected versions screen") };
        assert_eq!(v.path, "new.txt");
        assert!(v.page.is_none());
        assert!(!v.loading);
    }

    /// A versions screen for `path` with two loaded versions, the older one awaiting restore confirmation.
    fn versions_ready_to_restore(path: &str) -> (Model, VersionId, VersionId) {
        let mut page = history_page();
        let current = page.versions[0].id;
        let old = VersionId::generate();
        page.versions.push(craft_cloud_app::HistoryVersion {
            id: old,
            parent_version: Some(current),
            epoch: craft_cloud_proto::Epoch::generate(),
            seq: 2,
            size: 10,
            mtime_ms: 0,
            device: craft_cloud_app::DeviceId::generate(),
            created_ms: 0,
            save: false,
            pruned: false,
        });
        let mut m = model();
        m.screen = Screen::Versions(Versions {
            path: path.into(),
            page: Some(page),
            current_version: Some(current),
            loading: false,
            restoring: false,
            error: None,
            notice: None,
            confirm: Some(old),
            ticket: 0,
        });
        (m, current, old)
    }

    #[test]
    fn versions_restore_is_refused_while_a_page_loads_and_never_sticks() {
        let (mut m, current, old) = versions_ready_to_restore("file.txt");
        let more = m.update(Event::NextVersions);
        assert_eq!(more, vec![Effect::History { path: "file.txt".into(), cursor: Some("opaque-cursor".into()), ticket: 1 }]);
        assert!(m.update(Event::RestoreVersion).is_empty(), "restore must not start while a page loads");
        let Screen::Versions(v) = &m.screen else { panic!("expected versions screen") };
        assert!(v.loading && !v.restoring);
        assert_eq!(v.ticket, 1, "a refused restore must not take a ticket");
        assert!(m.update(Event::History { ticket: 1, result: Ok(history_page()) }).is_empty());
        let Screen::Versions(v) = &m.screen else { panic!("expected versions screen") };
        assert!(!v.loading);
        // Not stuck: the confirmation is still open and the restore now goes through.
        assert_eq!(v.confirm, Some(old));
        assert_eq!(
            m.update(Event::RestoreVersion),
            vec![Effect::Restore { path: "file.txt".into(), version: old, expected_version: current, ticket: 2 }]
        );
    }

    #[test]
    fn versions_page_request_during_restore_is_ignored_and_reload_follows() {
        let (mut m, current, old) = versions_ready_to_restore("file.txt");
        assert_eq!(
            m.update(Event::RestoreVersion),
            vec![Effect::Restore { path: "file.txt".into(), version: old, expected_version: current, ticket: 1 }]
        );
        assert!(m.update(Event::NextVersions).is_empty());
        assert!(m.update(Event::LoadVersions).is_empty());
        let Screen::Versions(v) = &m.screen else { panic!("expected versions screen") };
        assert!(v.restoring && !v.loading);
        let restored =
            craft_cloud_app::RestoreResponse { version: old, parent_version: current, seq: 5, epoch: craft_cloud_proto::Epoch::generate() };
        let effects = m.update(Event::Restored { ticket: 1, result: Ok(restored) });
        assert_eq!(effects, vec![Effect::History { path: "file.txt".into(), cursor: None, ticket: 2 }]);
        assert!(m.update(Event::History { ticket: 2, result: Ok(history_page()) }).is_empty());
        let Screen::Versions(v) = &m.screen else { panic!("expected versions screen") };
        assert!(!v.loading && !v.restoring);
    }

    #[test]
    fn versions_send_the_path_exactly_as_typed() {
        let (mut m, current, old) = versions_ready_to_restore(" Projects/file.txt ");
        assert_eq!(
            m.update(Event::RestoreVersion),
            vec![Effect::Restore { path: " Projects/file.txt ".into(), version: old, expected_version: current, ticket: 1 }]
        );
        m.update(Event::Restored { ticket: 1, result: Err(refused(api_codes::CONFLICT, "conflict")) });
        let Screen::Versions(v) = &m.screen else { panic!("expected versions screen") };
        assert!(v.loading, "the conflict reload is under way");
        let mut m = model();
        m.screen = Screen::Versions(Versions {
            path: " Projects/file.txt".into(),
            page: None,
            current_version: None,
            loading: false,
            restoring: false,
            error: None,
            notice: None,
            confirm: None,
            ticket: 0,
        });
        assert_eq!(m.update(Event::LoadVersions), vec![Effect::History { path: " Projects/file.txt".into(), cursor: None, ticket: 1 }]);
        m.update(Event::VersionsPath("   ".into()));
        assert_eq!(m.update(Event::LoadVersions), vec![Effect::History { path: "   ".into(), cursor: None, ticket: 3 }]);
        m.update(Event::VersionsPath(String::new()));
        assert!(m.update(Event::LoadVersions).is_empty());
    }

    fn refused(code: &str, message: &str) -> Error {
        Error::Refused { code: code.into(), message: message.into(), name_taken: None }
    }

    /// A model past the start, on the join form.
    fn on_join_form() -> Model {
        let mut m = model();
        assert_eq!(m.update(Event::Start), vec![Effect::Connect]);
        m.update(Event::Found(Ok(status(false))));
        assert!(matches!(m.screen, Screen::Join(_)), "{:?}", m.screen);
        // The tests below this one are about the join string; the sign-in has `on_sign_in_form`.
        m.update(Event::SetJoinMode(JoinMode::JoinString));
        m
    }

    /// A model past the start, on the sign-in form (the first screen) with a server, a user name and a password typed.
    fn on_sign_in_form() -> Model {
        let mut m = model();
        m.update(Event::Start);
        m.update(Event::Found(Ok(status(false))));
        let f = form(&mut m);
        assert_eq!(f.mode, JoinMode::SignIn, "sign-in is the first choice");
        f.server = "cloud.shadows.ch".into();
        f.user = "fabio".into();
        f.password = "correct horse battery".into();
        m
    }

    fn login_request(effects: &[Effect]) -> LoginRequest {
        match effects {
            [Effect::Login(request)] => request.clone(),
            other => panic!("no login: {other:?}"),
        }
    }

    fn second_step(effects: &[Effect]) -> SecondFactorLoginRequest {
        match effects {
            [Effect::LoginSecondFactor(request)] => request.clone(),
            other => panic!("no second step: {other:?}"),
        }
    }

    fn two_factor_required() -> Error {
        Error::RefusedWith {
            code: codes::TWO_FACTOR_REQUIRED.into(),
            message: "needs a code".into(),
            two_factor: Some(craft_cloud_app::TwoFactorChallenge { challenge: "challenge-secret".into(), expires_ms: 5 }),
            retry_after_secs: None,
        }
    }

    #[test]
    fn a_sign_in_sends_the_password_once_and_does_not_keep_it() {
        let mut m = on_sign_in_form();
        let request = login_request(&m.update(Event::SubmitJoin));
        assert_eq!(
            (request.server.as_str(), request.user.as_str(), request.password.as_str()),
            ("https://cloud.shadows.ch", "fabio", "correct horse battery")
        );
        assert_eq!(request.folder.as_deref(), Some(defaults().folder.as_str()));
        assert_eq!(request.device_name.as_deref(), Some("lptp1"));
        assert!(!request.adopt);
        let Screen::Joining { form: shown, password: None } = &m.screen else { panic!("{:?}", m.screen) };
        assert!(shown.password.is_empty(), "the password left the form with the request");
        assert!(!format!("{:?}", m.screen).contains("correct horse"));
        // It can't be sent twice.
        assert!(m.update(Event::SubmitJoin).is_empty());
        // Signed in: the signed-in view, as a join does.
        let effects = m.update(Event::LoggedIn(Ok(status(true))));
        assert!(matches!(m.screen, Screen::SignedIn(_)), "{:?}", m.screen);
        assert!(effects.contains(&Effect::SetAutostart(true)), "{effects:?}");
    }

    #[test]
    fn a_sign_in_with_a_field_missing_says_so_and_keeps_what_was_typed() {
        for (clear, said) in [("server", SERVER_EMPTY), ("user", USER_EMPTY), ("password", SIGN_IN_PASSWORD_EMPTY)] {
            let mut m = on_sign_in_form();
            let f = form(&mut m);
            match clear {
                "server" => f.server = "  ".into(),
                "user" => f.user.clear(),
                _ => f.password.clear(),
            }
            assert!(m.update(Event::SubmitJoin).is_empty());
            assert_eq!(form(&mut m).error.as_deref(), Some(said));
        }
        let mut m = on_sign_in_form();
        form(&mut m).folder = "relative/folder".into();
        assert!(m.update(Event::SubmitJoin).is_empty());
        let f = form(&mut m);
        assert_eq!(f.error.as_deref(), Some(FOLDER_NOT_ABSOLUTE));
        assert_eq!(f.password, "correct horse battery", "nothing was sent, so nothing was taken");
        // A scheme the user typed stays.
        let mut m = on_sign_in_form();
        form(&mut m).server = " http://127.0.0.1:18080/ ".into();
        assert_eq!(login_request(&m.update(Event::SubmitJoin)).server, "http://127.0.0.1:18080/");
    }

    #[test]
    fn a_sign_in_with_two_factor_asks_for_the_code_and_then_signs_in() {
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        assert!(m.update(Event::LoggedIn(Err(two_factor_required()))).is_empty());
        let Screen::SecondFactor(second) = &mut m.screen else { panic!("{:?}", m.screen) };
        assert_eq!(second.form.challenge.as_deref(), Some("challenge-secret"));
        assert!(second.form.password.is_empty() && second.password.is_none());
        let shown = format!("{:?}", m.screen);
        assert!(!shown.contains("challenge-secret") && !shown.contains("correct horse"), "{shown}");
        let Screen::SecondFactor(second) = &mut m.screen else { panic!() };
        // A code that is not six digits is said before anything is sent.
        second.code = "12".into();
        assert!(m.update(Event::SubmitSecondFactor).is_empty());
        let Screen::SecondFactor(second) = &mut m.screen else { panic!() };
        second.code = "654 321".into();
        let request = second_step(&m.update(Event::SubmitSecondFactor));
        assert_eq!(
            (request.server.as_str(), request.user.as_str(), request.challenge.as_str()),
            ("https://cloud.shadows.ch", "fabio", "challenge-secret")
        );
        assert_eq!((request.totp_code.as_deref(), request.recovery_code.as_deref()), (Some("654321"), None));
        assert_eq!(request.folder.as_deref(), Some(defaults().folder.as_str()));
        m.update(Event::LoggedIn(Ok(status(true))));
        assert!(matches!(m.screen, Screen::SignedIn(_)), "{:?}", m.screen);
    }

    #[test]
    fn a_wrong_code_keeps_the_challenge_for_the_next_try_and_a_recovery_code_works() {
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        m.update(Event::LoggedIn(Err(two_factor_required())));
        let Screen::SecondFactor(second) = &mut m.screen else { panic!("{:?}", m.screen) };
        second.code = "111111".into();
        m.update(Event::SubmitSecondFactor);
        m.update(Event::LoggedIn(Err(refused(codes::TWO_FACTOR_INVALID, "wrong"))));
        let Screen::SecondFactor(second) = &mut m.screen else { panic!("{:?}", m.screen) };
        assert!(second.error.as_deref().is_some_and(|e| e.contains("isn't right")), "{:?}", second.error);
        assert!(second.code.is_empty());
        assert_eq!(second.form.challenge.as_deref(), Some("challenge-secret"));
        second.recovery = true;
        second.code = "ABCD-EFGH-JKMN-PQRS".into();
        let request = second_step(&m.update(Event::SubmitSecondFactor));
        assert_eq!(request.challenge, "challenge-secret");
        assert_eq!((request.recovery_code.as_deref(), request.totp_code.as_deref()), (Some("ABCD-EFGH-JKMN-PQRS"), None));
    }

    #[test]
    fn a_locked_second_step_says_how_long_to_wait() {
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        m.update(Event::LoggedIn(Err(two_factor_required())));
        let Screen::SecondFactor(second) = &mut m.screen else { panic!("{:?}", m.screen) };
        second.code = "123456".into();
        m.update(Event::SubmitSecondFactor);
        let locked = |secs| Error::RefusedWith {
            code: codes::TWO_FACTOR_LOCKED.into(),
            message: "locked".into(),
            two_factor: None,
            retry_after_secs: Some(secs),
        };
        // The challenge (5 ms left here) ends before the lock does: back to the form, with the wait said, nothing kept.
        m.update(Event::LoggedIn(Err(locked(840))));
        let f = form(&mut m);
        assert_eq!(f.error.as_deref(), Some("Too many wrong codes. Try again in 14 minutes."));
        assert!(f.challenge.is_none() && f.password.is_empty());
        // A wait that is over before the challenge is: the code field stays, with the wait.
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        let long_lived = Error::RefusedWith {
            code: codes::TWO_FACTOR_REQUIRED.into(),
            message: "needs a code".into(),
            two_factor: Some(craft_cloud_app::TwoFactorChallenge { challenge: "challenge-secret".into(), expires_ms: 300_000 }),
            retry_after_secs: None,
        };
        m.update(Event::LoggedIn(Err(long_lived)));
        let Screen::SecondFactor(second) = &mut m.screen else { panic!("{:?}", m.screen) };
        second.code = "123456".into();
        m.update(Event::SubmitSecondFactor);
        m.update(Event::LoggedIn(Err(locked(30))));
        let Screen::SecondFactor(second) = &m.screen else { panic!("{:?}", m.screen) };
        assert_eq!(second.error.as_deref(), Some("Too many wrong codes. Try again in 30 seconds."));
        assert_eq!(second.form.challenge.as_deref(), Some("challenge-secret"));
    }

    #[test]
    fn a_busy_server_during_the_second_step_keeps_the_challenge_while_it_lives() {
        // The agent answers a server's 429 as `offline`; its message says what happened.
        let busy = || refused("offline", "the server is busy: too many sign-in attempts");
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        m.update(Event::LoggedIn(Err(two_factor_required())));
        m.now_ms = 1;
        let Screen::SecondFactor(second) = &mut m.screen else { panic!("{:?}", m.screen) };
        second.code = "123456".into();
        m.update(Event::SubmitSecondFactor);
        m.update(Event::LoggedIn(Err(busy())));
        let Screen::SecondFactor(second) = &m.screen else { panic!("{:?}", m.screen) };
        assert_eq!(second.error.as_deref(), Some("the server is busy: too many sign-in attempts"));
        assert_eq!(second.form.challenge.as_deref(), Some("challenge-secret"));
        // Past the challenge's end (5 ms): back to the form.
        let Screen::SecondFactor(second) = &mut m.screen else { panic!() };
        second.code = "123456".into();
        m.update(Event::SubmitSecondFactor);
        m.now_ms = 10;
        m.update(Event::LoggedIn(Err(busy())));
        let f = form(&mut m);
        assert_eq!(f.error.as_deref(), Some("the server is busy: too many sign-in attempts"));
        assert!(f.challenge.is_none());
    }

    #[test]
    fn a_server_without_a_scheme_is_https_unless_it_is_this_computer() {
        for (typed, sent) in [
            ("cloud.shadows.ch", "https://cloud.shadows.ch"),
            ("cloud.shadows.ch:8443/cloud", "https://cloud.shadows.ch:8443/cloud"),
            ("localhost:18080", "http://localhost:18080"),
            ("LOCALHOST", "http://LOCALHOST"),
            ("127.0.0.1:18080", "http://127.0.0.1:18080"),
            ("127.3.4.5", "http://127.3.4.5"),
            ("[::1]:18080", "http://[::1]:18080"),
            ("192.168.1.5:18080", "https://192.168.1.5:18080"),
            ("localhost.example.com", "https://localhost.example.com"),
            ("127.0.0.1.example.com", "https://127.0.0.1.example.com"),
            ("http://nas.local:18080", "http://nas.local:18080"),
        ] {
            let mut m = on_sign_in_form();
            form(&mut m).server = typed.into();
            assert_eq!(login_request(&m.update(Event::SubmitJoin)).server, sent, "{typed}");
        }
    }

    #[test]
    fn an_expired_challenge_goes_back_to_the_form_without_a_password() {
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        m.update(Event::LoggedIn(Err(two_factor_required())));
        let Screen::SecondFactor(second) = &mut m.screen else { panic!("{:?}", m.screen) };
        second.code = "123456".into();
        m.update(Event::SubmitSecondFactor);
        m.update(Event::LoggedIn(Err(refused(codes::TWO_FACTOR_EXPIRED, "gone"))));
        let f = form(&mut m);
        assert_eq!(f.error.as_deref(), Some(CHALLENGE_EXPIRED));
        assert!(f.challenge.is_none() && f.password.is_empty() && f.user == "fabio");
    }

    #[test]
    fn a_challenge_missing_from_the_answer_is_said() {
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        m.update(Event::LoggedIn(Err(refused(codes::TWO_FACTOR_REQUIRED, "needs a code"))));
        assert_eq!(form(&mut m).error.as_deref(), Some(NO_CHALLENGE));
    }

    #[test]
    fn sign_in_refusals_are_explained_and_leave_no_password() {
        let rate_limited = Error::RefusedWith { code: "rate_limited".into(), message: "m".into(), two_factor: None, retry_after_secs: Some(120) };
        for (error, said) in [
            (refused("wrong_password", "wrong user name or password"), "That password isn't right."),
            (rate_limited, "Too many tries. Try again in 2 minutes."),
            (refused("offline", "the server is busy: too many sign-in attempts"), "the server is busy: too many sign-in attempts"),
        ] {
            let mut m = on_sign_in_form();
            m.update(Event::SubmitJoin);
            m.update(Event::LoggedIn(Err(error)));
            let f = form(&mut m);
            assert_eq!(f.error.as_deref(), Some(said));
            assert!(f.password.is_empty());
            assert_eq!((f.server.as_str(), f.user.as_str()), ("cloud.shadows.ch", "fabio"), "what was typed stays");
        }
        // Said as the agent says it when there is no better text.
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        m.update(Event::LoggedIn(Err(refused("usage", "cloud.shadows.ch is not a URL"))));
        assert_eq!(form(&mut m).error.as_deref(), Some("cloud.shadows.ch is not a URL"));
    }

    #[test]
    fn a_sign_in_to_a_folder_with_files_asks_and_then_needs_the_password_again() {
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        m.update(Event::LoggedIn(Err(refused(codes::FOLDER_HAS_FILES, "has files"))));
        let f = form(&mut m);
        assert!(f.ask_adopt && f.password.is_empty());
        // Sync them: the password is not kept, so it is asked for again, and then goes with `adopt`.
        assert!(m.update(Event::SyncExistingFiles(true)).is_empty());
        let f = form(&mut m);
        assert_eq!(f.error.as_deref(), Some(PASSWORD_AGAIN));
        assert!(!f.ask_adopt);
        f.password = "correct horse battery".into();
        let request = login_request(&m.update(Event::SubmitJoin));
        assert!(request.adopt);
        // Another folder is asked about again.
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        m.update(Event::LoggedIn(Err(refused(codes::FOLDER_HAS_FILES, "has files"))));
        m.update(Event::SyncExistingFiles(false));
        form(&mut m).password = "x".into();
        assert!(!login_request(&m.update(Event::SubmitJoin)).adopt);
    }

    #[test]
    fn going_back_from_the_code_drops_the_challenge() {
        let mut m = on_sign_in_form();
        m.update(Event::SubmitJoin);
        m.update(Event::LoggedIn(Err(two_factor_required())));
        m.update(Event::CancelSecondFactor);
        let f = form(&mut m);
        assert!(f.challenge.is_none() && f.password.is_empty());
        assert_eq!((f.server.as_str(), f.user.as_str()), ("cloud.shadows.ch", "fabio"));
    }

    #[test]
    fn switching_between_sign_in_and_join_string_keeps_the_folder_and_drops_the_password() {
        let mut m = on_sign_in_form();
        let f = form(&mut m);
        f.folder = "/data/cloud".into();
        f.device_name = "studio".into();
        f.error = Some("old".into());
        m.update(Event::SetJoinMode(JoinMode::JoinString));
        let f = form(&mut m);
        assert_eq!((f.mode, f.folder.as_str(), f.device_name.as_str()), (JoinMode::JoinString, "/data/cloud", "studio"));
        assert!(f.password.is_empty() && f.error.is_none());
        assert_eq!(f.user, "fabio", "the sign-in's other fields are kept for the way back");
        m.update(Event::SetJoinMode(JoinMode::SignIn));
        assert_eq!(form(&mut m).folder, "/data/cloud");
        // Only on the form.
        let mut m = signed_in();
        assert!(m.update(Event::SetJoinMode(JoinMode::JoinString)).is_empty());
    }

    #[test]
    fn the_rail_shows_what_is_known_and_never_a_probe() {
        let mut m = on_sign_in_form();
        let steps = form(&mut m).rail();
        assert_eq!((steps[0].title, steps[0].detail.as_str()), ("Server", "cloud.shadows.ch"));
        assert_eq!(steps[1].detail.as_str(), "fabio");
        assert_eq!(steps[2].detail, defaults().folder);
        let f = form(&mut m);
        f.server.clear();
        f.user.clear();
        f.folder.clear();
        let steps = f.rail();
        assert_eq!(steps[0].detail, "Where your Craft Cloud lives");
        assert_eq!(steps[2].detail, "Where your files live on this computer");
        m.update(Event::SetJoinMode(JoinMode::JoinString));
        form(&mut m).join = "https://cloud.shadows.ch/join/k7m4-q9tx".into();
        let steps = form(&mut m).rail();
        assert_eq!((steps[0].detail.as_str(), steps[1].detail.as_str()), ("cloud.shadows.ch", "Join string"));
    }

    fn states(m: &Model) -> Vec<RailState> {
        let (_, rail) = m.first_run_rail().expect("a screen of the first start");
        rail.iter().map(|s| s.state).collect()
    }

    const DONE: RailState = RailState::Done;
    const NOW: RailState = RailState::Current;
    const LATER: RailState = RailState::Later;

    /// Continue on the sign-in form three times: the sign-in is sent.
    fn signed_in_through_the_steps(m: &mut Model) {
        for _ in 0..3 {
            m.update(Event::JoinNext);
        }
    }

    #[test]
    fn the_steps_go_forward_one_page_at_a_time_and_the_rail_follows_in_sequence() {
        let mut m = on_sign_in_form();
        assert_eq!(form(&mut m).step, JoinStep::Server, "the first start begins at the server");
        // Typed values never move the rail: only the step does.
        assert_eq!(states(&m), [NOW, LATER, LATER]);
        assert!(m.update(Event::JoinNext).is_empty());
        assert_eq!((form(&mut m).step, form(&mut m).error.clone()), (JoinStep::Account, None));
        assert_eq!(states(&m), [DONE, NOW, LATER]);
        assert!(m.update(Event::JoinNext).is_empty());
        assert_eq!(form(&mut m).step, JoinStep::Folder);
        assert_eq!(states(&m), [DONE, DONE, NOW]);
        // The last step's Continue signs in exactly as SubmitJoin does.
        let request = login_request(&m.update(Event::JoinNext));
        assert_eq!(
            (request.server.as_str(), request.user.as_str(), request.password.as_str()),
            ("https://cloud.shadows.ch", "fabio", "correct horse battery")
        );
        assert!(matches!(m.screen, Screen::Joining { .. }), "{:?}", m.screen);
        // Nothing is sent twice, and a step button does nothing then.
        assert!(m.update(Event::JoinNext).is_empty() && m.update(Event::JoinBack).is_empty());
    }

    #[test]
    fn a_step_is_not_left_forward_with_its_field_missing() {
        let mut m = on_sign_in_form();
        form(&mut m).server = "  ".into();
        assert!(m.update(Event::JoinNext).is_empty());
        let f = form(&mut m);
        assert_eq!((f.step, f.error.as_deref()), (JoinStep::Server, Some(SERVER_EMPTY)));
        f.server = "cloud.shadows.ch".into();
        m.update(Event::JoinNext);
        assert_eq!((form(&mut m).step, form(&mut m).error.clone()), (JoinStep::Account, None), "the message goes with a fixed field");

        form(&mut m).user.clear();
        m.update(Event::JoinNext);
        assert_eq!((form(&mut m).step, form(&mut m).error.as_deref()), (JoinStep::Account, Some(USER_EMPTY)));
        let f = form(&mut m);
        f.user = "fabio".into();
        f.password.clear();
        m.update(Event::JoinNext);
        assert_eq!((form(&mut m).step, form(&mut m).error.as_deref()), (JoinStep::Account, Some(SIGN_IN_PASSWORD_EMPTY)));
        assert_eq!(states(&m), [DONE, NOW, LATER], "an error keeps the current step current");

        // The join string is read on the account step, with the message of the whole form.
        let mut m = on_join_form();
        form(&mut m).step = JoinStep::Account;
        form(&mut m).join = "not a code".into();
        m.update(Event::JoinNext);
        assert_eq!(form(&mut m).step, JoinStep::Account);
        assert!(form(&mut m).error.is_some());
        form(&mut m).join = "cloud.shadows.ch/K7M4-Q9TX".into();
        m.update(Event::JoinNext);
        assert_eq!((form(&mut m).step, form(&mut m).error.clone()), (JoinStep::Folder, None));
        // The last step checks the folder and stays.
        form(&mut m).folder = "relative/folder".into();
        assert!(m.update(Event::JoinNext).is_empty());
        assert_eq!((form(&mut m).step, form(&mut m).error.as_deref()), (JoinStep::Folder, Some(FOLDER_NOT_ABSOLUTE)));
        assert_eq!(states(&m), [DONE, DONE, NOW]);
    }

    #[test]
    fn a_join_string_does_not_need_a_server_address_on_the_first_step() {
        let mut m = on_join_form();
        assert_eq!(form(&mut m).step, JoinStep::Server);
        assert!(form(&mut m).server.is_empty());
        m.update(Event::JoinNext);
        assert_eq!((form(&mut m).step, form(&mut m).error.clone()), (JoinStep::Account, None));
    }

    #[test]
    fn back_goes_to_the_step_before_and_keeps_what_was_typed() {
        let mut m = on_sign_in_form();
        m.update(Event::JoinNext);
        m.update(Event::JoinNext);
        form(&mut m).folder = "/data/cloud".into();
        form(&mut m).device_name = "studio".into();
        m.update(Event::JoinBack);
        assert_eq!(form(&mut m).step, JoinStep::Account);
        assert_eq!(states(&m), [DONE, NOW, LATER]);
        let f = form(&mut m);
        assert_eq!((f.password.as_str(), f.user.as_str(), f.server.as_str()), ("correct horse battery", "fabio", "cloud.shadows.ch"));
        m.update(Event::JoinBack);
        assert_eq!(form(&mut m).step, JoinStep::Server);
        // There is nothing before the first step.
        assert!(m.update(Event::JoinBack).is_empty());
        assert_eq!(form(&mut m).step, JoinStep::Server);
        m.update(Event::JoinNext);
        m.update(Event::JoinNext);
        let f = form(&mut m);
        assert_eq!((f.folder.as_str(), f.device_name.as_str()), ("/data/cloud", "studio"));
    }

    #[test]
    fn a_problem_puts_the_form_on_the_step_whose_field_it_is_about() {
        // The password is gone after a try: a wrong one is typed again on the account step.
        let mut m = on_sign_in_form();
        signed_in_through_the_steps(&mut m);
        m.update(Event::LoggedIn(Err(refused("wrong_password", "wrong user name or password"))));
        let f = form(&mut m);
        assert_eq!(f.step, JoinStep::Account);
        assert!(f.error.is_some() && f.password.is_empty());
        assert_eq!(states(&m), [DONE, NOW, LATER], "never every step done after an error");
        // The folder has files: the question is on the folder step.
        let mut m = on_sign_in_form();
        signed_in_through_the_steps(&mut m);
        m.update(Event::LoggedIn(Err(refused(codes::FOLDER_HAS_FILES, "has files"))));
        let f = form(&mut m);
        assert_eq!((f.step, f.ask_adopt), (JoinStep::Folder, true));
        // Agreeing needs the password again (it is not kept): the account step.
        m.update(Event::SyncExistingFiles(true));
        let f = form(&mut m);
        assert_eq!((f.step, f.error.as_deref()), (JoinStep::Account, Some(PASSWORD_AGAIN)));
    }

    #[test]
    fn the_code_and_the_new_password_are_pages_of_the_account_step() {
        let mut m = on_sign_in_form();
        signed_in_through_the_steps(&mut m);
        m.update(Event::LoggedIn(Err(two_factor_required())));
        assert!(matches!(m.screen, Screen::SecondFactor(_)), "{:?}", m.screen);
        assert_eq!(m.first_run_rail().map(|(n, _)| n), Some(1));
        assert_eq!(states(&m), [DONE, NOW, LATER]);
        // Back from the code is the account step, with the password to type again; the other values are kept.
        m.update(Event::CancelSecondFactor);
        let f = form(&mut m);
        assert_eq!((f.step, f.user.as_str(), f.server.as_str()), (JoinStep::Account, "fabio", "cloud.shadows.ch"));
        assert!(f.password.is_empty() && f.challenge.is_none());

        // An invite that wants a new password.
        let mut m = on_join_form();
        form(&mut m).join = "cloud.shadows.ch/K7M4-Q9TX-K7M4-Q9TX".into();
        form(&mut m).step = JoinStep::Folder;
        m.update(Event::JoinNext);
        m.update(Event::Joined(Err(refused(codes::PASSWORD_REQUIRED, "x"))));
        assert!(matches!(m.screen, Screen::PasswordNeeded(_)), "{:?}", m.screen);
        assert_eq!(states(&m), [DONE, NOW, LATER]);
        m.update(Event::CancelPassword);
        assert_eq!(form(&mut m).step, JoinStep::Folder, "Back from the password is the page it came from");
    }

    #[test]
    fn the_step_buttons_only_work_on_the_join_form() {
        let mut m = signed_in();
        assert!(m.update(Event::JoinNext).is_empty() && m.update(Event::JoinBack).is_empty());
        assert!(matches!(m.screen, Screen::SignedIn(_)));
        assert!(m.first_run_rail().is_none());
    }

    #[test]
    fn secrets_of_the_form_stay_out_of_debug_output() {
        let f = JoinForm {
            password: "pw-secret-1".into(),
            challenge: Some("challenge-secret".into()),
            join: "cloud.shadows.ch/K7M4-Q9TX".into(),
            user: "fabio".into(),
            ..JoinForm::default()
        };
        let shown = format!("{f:?}");
        assert!(!shown.contains("pw-secret") && !shown.contains("challenge-secret") && !shown.contains("K7M4"), "{shown}");
        assert!(shown.contains("fabio"));
    }

    fn signed_in() -> Model {
        let mut m = model();
        m.update(Event::Start);
        let effects = m.update(Event::Found(Ok(status(true))));
        assert!(effects.contains(&Effect::Devices), "{effects:?}");
        m
    }

    fn form(m: &mut Model) -> &mut JoinForm {
        match &mut m.screen {
            Screen::Join(form) => form,
            other => panic!("not the join form: {other:?}"),
        }
    }

    fn join_request(effects: &[Effect]) -> JoinRequest {
        match effects {
            [Effect::Join(request)] => request.clone(),
            other => panic!("no join: {other:?}"),
        }
    }

    #[test]
    fn not_running_starts_the_agent_and_says_so_if_it_doesnt_start() {
        let mut m = model();
        m.update(Event::Start);
        assert_eq!(m.update(Event::Found(Err(Error::NotRunning("no local-api.json".into())))), vec![Effect::StartAgent]);
        assert_eq!(m.screen, Screen::Starting);
        assert!(m.update(Event::Tick { now_ms: 1 }).is_empty(), "no polls while it starts");
        m.update(Event::AgentStarted(Err("it didn't answer within 5 seconds".into())));
        let Screen::NotRunning(problem) = &m.screen else { panic!("{:?}", m.screen) };
        assert_eq!(problem.line, DIDNT_START);
        assert!(problem.details.contains("run.log") && problem.details.contains("5 seconds"), "{}", problem.details);
        assert_eq!(problem.action, ProblemAction::TryAgain);
        // Try again starts it again, and this time it answers.
        assert_eq!(m.update(Event::TryAgain), vec![Effect::StartAgent]);
        assert_eq!(m.screen, Screen::Starting);
        m.update(Event::AgentStarted(Ok(status(false))));
        assert!(matches!(m.screen, Screen::Join(_)));
    }

    #[test]
    fn starting_and_not_signed_in_show_the_join_form_with_its_defaults() {
        let mut m = model();
        assert_eq!(m.screen, Screen::Starting);
        let mut m2 = on_join_form();
        assert_eq!(form(&mut m2).folder, defaults().folder);
        assert_eq!(form(&mut m2).device_name, "lptp1");
        // The poll keeps it there, and keeps what was typed.
        form(&mut m2).join = "cloud.shadows.ch/K7M4".into();
        assert_eq!(m2.update(Event::Tick { now_ms: 2_000 }), vec![Effect::Status]);
        assert!(m2.update(Event::Tick { now_ms: 4_000 }).is_empty(), "one status call at a time");
        m2.update(Event::Polled(Ok(status(false))));
        assert_eq!(form(&mut m2).join, "cloud.shadows.ch/K7M4");
        m.update(Event::Start);
        m.update(Event::Found(Ok(status(false))));
        assert!(matches!(m.screen, Screen::Join(_)));
    }

    /// A pasted link, spaces and lower case: the join string as the agent reads it, and the form shows it so.
    #[test]
    fn a_pasted_link_with_spaces_and_lower_case_is_read() {
        let mut m = on_join_form();
        form(&mut m).join = "  https://cloud.shadows.ch/join/k7m4-q9tx \n".into();
        let request = join_request(&m.update(Event::SubmitJoin));
        assert_eq!(request.join, "cloud.shadows.ch/K7M4-Q9TX");
        assert_eq!(request.folder.as_deref(), Some(defaults().folder.as_str()));
        assert_eq!((request.device_name.as_deref(), request.adopt, request.password.as_deref()), (Some("lptp1"), false, None));
        let Screen::Joining { form: shown, password: None } = &m.screen else { panic!("{:?}", m.screen) };
        assert_eq!(shown.join, "cloud.shadows.ch/K7M4-Q9TX");
        // Joining: it can't be sent twice.
        assert!(m.update(Event::SubmitJoin).is_empty());
        let mut m = on_join_form();
        form(&mut m).join = "cloud.shadows.ch/k7m4 q9tx".into();
        assert_eq!(join_request(&m.update(Event::SubmitJoin)).join, "cloud.shadows.ch/K7M4-Q9TX");
    }

    #[test]
    fn a_join_string_that_doesnt_read_is_said_before_anything_is_sent() {
        let mut m = on_join_form();
        form(&mut m).join = "K7M4-Q9TX".into();
        assert!(m.update(Event::SubmitJoin).is_empty());
        let error = form(&mut m).error.clone().unwrap();
        assert!(error.starts_with("Put the server's address in front"), "{error}");
        form(&mut m).join = "cloud.shadows.ch/K7M4-Q9TX".into();
        form(&mut m).folder = "Craft Cloud".into();
        assert!(m.update(Event::SubmitJoin).is_empty());
        assert_eq!(form(&mut m).error.as_deref(), Some(FOLDER_NOT_ABSOLUTE));
    }

    /// A folder with files in it: the agent says so before the code is spent; the form asks, and a yes sends the
    /// join again with `adopt` (for that folder only).
    #[test]
    fn a_folder_with_files_asks_and_then_adopts() {
        let mut m = on_join_form();
        form(&mut m).join = "cloud.shadows.ch/K7M4-Q9TX".into();
        m.update(Event::SubmitJoin);
        m.update(Event::Joined(Err(refused(codes::FOLDER_HAS_FILES, "/home/ana/Craft Cloud already has files in it; pass --adopt"))));
        assert!(form(&mut m).ask_adopt);
        assert_eq!(form(&mut m).error, None);
        let request = join_request(&m.update(Event::SyncExistingFiles(true)));
        assert!(request.adopt);
        assert!(matches!(m.screen, Screen::Joining { .. }));
        // Saying no leaves the form, to choose another folder; a changed folder is asked about again.
        let mut m = on_join_form();
        form(&mut m).join = "cloud.shadows.ch/K7M4-Q9TX".into();
        m.update(Event::SubmitJoin);
        m.update(Event::Joined(Err(refused(codes::FOLDER_HAS_FILES, "has files"))));
        assert!(m.update(Event::SyncExistingFiles(false)).is_empty());
        assert!(!form(&mut m).ask_adopt);
        form(&mut m).adopt_for = Some("/elsewhere".into());
        assert!(!join_request(&m.update(Event::SubmitJoin)).adopt);
    }

    /// Signed in meanwhile (another program joined first): the join's refusal moves on to the signed-in view at the
    /// next status, not a "Joining…" that never ends.
    #[test]
    fn a_join_refused_as_already_signed_in_moves_on() {
        let mut m = on_join_form();
        form(&mut m).join = "cloud.shadows.ch/K7M4-Q9TX".into();
        m.update(Event::SubmitJoin);
        assert_eq!(m.update(Event::Joined(Err(refused(codes::ALREADY_SIGNED_IN, "already signed in as fabio")))), vec![Effect::Status]);
        assert!(!matches!(m.screen, Screen::Joining { .. }), "{:?}", m.screen);
        m.update(Event::Polled(Ok(status(true))));
        assert!(matches!(m.screen, Screen::SignedIn(_)), "{:?}", m.screen);
    }

    #[test]
    fn change_picks_a_folder_with_the_os_picker() {
        let mut m = on_join_form();
        assert_eq!(m.update(Event::ChangeFolder), vec![Effect::PickFolder(defaults().folder)]);
        m.update(Event::FolderPicked(None));
        assert_eq!(form(&mut m).folder, defaults().folder, "cancelled");
        form(&mut m).ask_adopt = true;
        m.update(Event::FolderPicked(Some("/media/ana/Cloud".into())));
        assert_eq!(form(&mut m).folder, "/media/ana/Cloud");
        assert!(!form(&mut m).ask_adopt, "another folder: asked about again if it has files");
    }

    /// An invite to a new account: a password, twice, that must match; the server's `weak_password` message is
    /// shown; then it joins.
    #[test]
    fn an_invite_asks_for_a_password_twice() {
        let mut m = on_join_form();
        form(&mut m).join = "cloud.shadows.ch/K7M4Q9TXK7M4Q9TX".into();
        m.update(Event::SubmitJoin);
        m.update(Event::Joined(Err(refused(codes::PASSWORD_REQUIRED, "this invite is for a new account"))));
        let pw = |m: &mut Model| match &mut m.screen {
            Screen::PasswordNeeded(pw) => pw.clone(),
            other => panic!("{other:?}"),
        };
        assert!(m.update(Event::SubmitPassword).is_empty());
        assert_eq!(pw(&mut m).error.as_deref(), Some(PASSWORD_EMPTY));
        if let Screen::PasswordNeeded(p) = &mut m.screen {
            p.password = "short".into();
            p.again = "shirt".into();
        }
        assert!(m.update(Event::SubmitPassword).is_empty());
        assert_eq!(pw(&mut m).error.as_deref(), Some(PASSWORDS_DIFFER));
        if let Screen::PasswordNeeded(p) = &mut m.screen {
            p.again = "short".into();
        }
        let request = join_request(&m.update(Event::SubmitPassword));
        assert_eq!(request.password.as_deref(), Some("short"));
        assert!(matches!(m.screen, Screen::Joining { password: Some(_), .. }));
        assert!(!format!("{:?}", m.screen).contains("short"), "the password stays out of debug output");
        m.update(Event::Joined(Err(refused(codes::WEAK_PASSWORD, "the password must be at least 12 characters long"))));
        let back = pw(&mut m);
        assert_eq!(back.error.as_deref(), Some("the password must be at least 12 characters long"));
        assert!(back.password.is_empty() && back.again.is_empty());
        // Cancel goes back to the form as it was.
        m.update(Event::CancelPassword);
        assert_eq!(form(&mut m).join, "cloud.shadows.ch/K7M4-Q9TX-K7M4-Q9TX");
    }

    #[test]
    fn an_expired_code_says_the_specs_sentence() {
        let mut m = on_join_form();
        form(&mut m).join = "cloud.shadows.ch/K7M4-Q9TX".into();
        m.update(Event::SubmitJoin);
        m.update(Event::Joined(Err(refused(codes::INVALID_CODE, "whatever the agent says"))));
        assert_eq!(form(&mut m).error.as_deref(), Some(INVALID_CODE));
        // Other refusals show the agent's message (it names the address tried, or the certificate).
        m.update(Event::SubmitJoin);
        let untrusted = "This computer doesn't trust cloud.shadows.ch's certificate. If the server has a certificate authority …";
        m.update(Event::Joined(Err(refused(codes::UNTRUSTED_CERTIFICATE, untrusted))));
        assert_eq!(form(&mut m).error.as_deref(), Some(untrusted));
    }

    /// Joined: the signed-in view, start at login turned on for the agent, the computers asked for; then the app's own
    /// entry follows the agent's.
    #[test]
    fn joining_signs_in_and_turns_on_start_at_login() {
        let mut m = on_join_form();
        form(&mut m).join = "cloud.shadows.ch/K7M4-Q9TX".into();
        m.update(Event::SubmitJoin);
        assert!(m.update(Event::Tick { now_ms: 2_000 }).contains(&Effect::Status));
        // A poll while joining doesn't move the screen.
        m.update(Event::Polled(Ok(status(false))));
        assert!(matches!(m.screen, Screen::Joining { .. }));
        let joined = Status { autostart: Some(false), ..status(true) };
        let effects = m.update(Event::Joined(Ok(joined.clone())));
        assert!(effects.contains(&Effect::SetAutostart(true)) && effects.contains(&Effect::Devices), "{effects:?}");
        assert!(matches!(m.screen, Screen::SignedIn(_)));
        let effects = m.update(Event::AutostartSet(Ok(Status { autostart: Some(true), ..joined.clone() })));
        assert_eq!(effects, vec![Effect::AppAutostart(true)]);
        assert_eq!(m.autostart_problem, None);
        // Said once if it fails, with Try again; the join stands.
        let mut m = on_join_form();
        form(&mut m).join = "cloud.shadows.ch/K7M4-Q9TX".into();
        m.update(Event::SubmitJoin);
        m.update(Event::Joined(Ok(joined)));
        m.update(Event::AutostartSet(Err(refused(codes::AUTOSTART_FAILED, "can't write the unit"))));
        assert!(m.autostart_problem.as_deref().is_some_and(|p| p.starts_with(NO_SYNC_AFTER_RESTART)));
        assert!(matches!(m.screen, Screen::SignedIn(_)));
        assert_eq!(m.update(Event::RetryAutostart), vec![Effect::SetAutostart(true)]);
    }

    #[test]
    fn signed_in_shows_the_account_and_follows_the_status() {
        let mut m = signed_in();
        let Screen::SignedIn(o) = &m.screen else { panic!() };
        assert_eq!(o.status.account.as_deref(), Some("fabio"));
        let devices = vec![
            DeviceInfo {
                id: DeviceId::generate(),
                name: "lptp1".into(),
                created_ms: 1,
                last_used_ms: 2,
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
                created_ms: 1,
                last_used_ms: 3,
                revoked: false,
                current: false,
                client_kind: String::new(),
                platform: String::new(),
                app_version: String::new(),
                revoked_ms: None,
                revoke_reason: None,
            },
        ];
        m.update(Event::Devices(Ok(devices.clone())));
        // This computer can't be removed from here; another one can, once at a time.
        assert!(m.update(Event::RemoveDevice(devices[0].id)).is_empty());
        assert_eq!(m.update(Event::RemoveDevice(devices[1].id)), vec![Effect::RemoveDevice(devices[1].id)]);
        assert!(m.update(Event::RemoveDevice(devices[1].id)).is_empty());
        m.update(Event::Devices(Ok(devices[..1].to_vec())));
        assert_eq!(m.overview().unwrap().devices.as_ref().unwrap().len(), 1);
        assert_eq!(m.update(Event::OpenFolder), vec![Effect::OpenFolder("/home/ana/Craft Cloud".into())]);
        let offline = Status { online: false, run_state: RunState::Retrying { message: "can't reach it".into() }, ..status(true) };
        m.update(Event::Polled(Ok(offline.clone())));
        assert_eq!(m.overview().unwrap().status, offline);
        // An agent already signed in when the app starts: the signed-in view at once, nothing to redo.
        assert!(matches!(signed_in().screen, Screen::SignedIn(_)));
    }

    #[test]
    fn pause_and_resume_go_through_the_agent_once_at_a_time() {
        let mut m = signed_in();
        assert_eq!(m.update(Event::Pause), vec![Effect::Pause]);
        assert!(m.update(Event::Pause).is_empty());
        let paused = Status { run_state: RunState::PausedByUser, ..status(true) };
        m.update(Event::Answered(Ok(paused.clone())));
        assert_eq!(m.busy, None);
        assert_eq!(m.overview().unwrap().status.run_state, RunState::PausedByUser);
        assert_eq!(m.update(Event::Resume), vec![Effect::Resume]);
        m.update(Event::Answered(Err(Error::Io("timed out".into()))));
        assert!(m.overview().unwrap().error.is_some());
    }

    #[test]
    fn adding_a_computer_shows_a_code_until_it_is_used_or_expires() {
        let mut m = signed_in();
        assert_eq!(m.update(Event::AddComputer), vec![Effect::Pairing]);
        let id = Uuid::now_v7();
        let info = PairingInfo {
            id,
            join: "cloud.shadows.ch/K7M4-Q9TX".into(),
            link: "https://cloud.shadows.ch/join/K7M4-Q9TX".into(),
            expires_ms: 600_000,
        };
        m.update(Event::PairingMade(Ok(info)));
        let effects = m.update(Event::Tick { now_ms: 2_000 });
        assert!(effects.contains(&Effect::PairingState(id)) && effects.contains(&Effect::Status), "{effects:?}");
        assert!(!m.update(Event::Tick { now_ms: 4_000 }).contains(&Effect::PairingState(id)), "one at a time");
        m.update(Event::PairingPolled(Ok(PairingStatus { state: PairingState::Waiting, device_name: None })));
        let Screen::AddingComputer(adding) = &m.screen else { panic!() };
        assert_eq!(adding.state, AddState::Waiting);
        assert!(m.update(Event::Tick { now_ms: 6_000 }).contains(&Effect::PairingState(id)));
        let effects = m.update(Event::PairingPolled(Ok(PairingStatus { state: PairingState::Used, device_name: Some("lptp1".into()) })));
        assert_eq!(effects, vec![Effect::Devices]);
        let Screen::AddingComputer(adding) = &m.screen else { panic!() };
        assert_eq!(adding.state, AddState::Joined("lptp1".into()));
        assert_eq!(m.update(Event::CloseAdding), vec![Effect::Devices], "a used code isn't cancelled");
        assert!(matches!(m.screen, Screen::SignedIn(_)));

        // The countdown runs out.
        m.update(Event::AddComputer);
        m.update(Event::PairingMade(Ok(PairingInfo { id, join: "j".into(), link: "l".into(), expires_ms: 600_000 })));
        m.update(Event::Tick { now_ms: 600_001 });
        let Screen::AddingComputer(adding) = &m.screen else { panic!() };
        assert_eq!(adding.state, AddState::Expired);
        // Closed while waiting: the code is cancelled.
        m.update(Event::CloseAdding);
        m.update(Event::AddComputer);
        m.update(Event::PairingMade(Ok(PairingInfo { id, join: "j".into(), link: "l".into(), expires_ms: 600_000 })));
        assert_eq!(m.update(Event::CloseAdding), vec![Effect::CancelPairing(id), Effect::Devices]);
    }

    /// Removed on another computer: the next status says so, and Join again forgets the account here.
    #[test]
    fn a_removed_computer_says_so_and_joins_again() {
        let mut m = signed_in();
        let removed = Status {
            removed: true,
            run_state: RunState::Paused { reason: "the server no longer accepts this device".into(), reason_code: None },
            ..status(true)
        };
        m.update(Event::Polled(Ok(removed)));
        assert_eq!(m.screen, Screen::Removed);
        assert_eq!(m.update(Event::ForgetAccount), vec![Effect::Logout { local: true }]);
        assert!(m.update(Event::ForgetAccount).is_empty());
        let effects = m.update(Event::SignedOut(Ok(status(false))));
        assert!(matches!(m.screen, Screen::Join(_)));
        assert!(effects.contains(&Effect::AppAutostart(false)), "{effects:?}");
    }

    /// A problem the agent can't put right by itself (a damaged account file): one line, the whole text under Details,
    /// and a way to start over.
    #[test]
    fn a_problem_found_by_the_agent_is_one_line_with_details() {
        let mut m = model();
        m.update(Event::Start);
        let reason = "/home/ana/.local/share/craft-cloud-agent/account.json is damaged (expected value at line 1); move it away. Then sign in again";
        m.update(Event::Found(Ok(Status { run_state: RunState::Paused { reason: reason.into(), reason_code: None }, ..status(false) })));
        let Screen::Error(problem) = &m.screen else { panic!("{:?}", m.screen) };
        assert_eq!(problem.line, "/home/ana/.local/share/craft-cloud-agent/account.json is damaged (expected value at line 1); move it away.");
        assert_eq!(problem.details, reason);
        assert_eq!(problem.action, ProblemAction::ForgetAccount);
        assert_eq!(m.update(Event::ForgetAccount), vec![Effect::Logout { local: true }]);
        m.update(Event::SignedOut(Ok(status(false))));
        assert!(matches!(m.screen, Screen::Join(_)));
        // Another version of the agent: nothing to do but say so.
        let mut m = model();
        m.update(Event::Found(Err(Error::Version(9))));
        assert!(matches!(&m.screen, Screen::Error(Problem { action: ProblemAction::None, .. })));
    }

    /// Review focus: the agent goes away while the window is open: the next poll says so (never a frozen window);
    /// Try again starts it.
    #[test]
    fn an_agent_that_goes_away_is_said_at_the_next_poll() {
        let mut m = signed_in();
        m.update(Event::Tick { now_ms: 2_000 });
        m.update(Event::Polled(Err(Error::NotRunning("can't reach it: connection refused".into()))));
        let Screen::NotRunning(problem) = &m.screen else { panic!("{:?}", m.screen) };
        assert_eq!(problem.line, STOPPED);
        assert_eq!(m.status, None);
        // Polling goes on (an agent started at login comes back by itself), and so does Try again.
        assert_eq!(m.update(Event::Tick { now_ms: 4_000 }), vec![Effect::Status]);
        m.update(Event::Polled(Ok(status(true))));
        assert!(matches!(m.screen, Screen::SignedIn(_)));
        // A call that broke once isn't taken for a stop; twice is.
        m.update(Event::Polled(Err(Error::Io("reset".into()))));
        assert!(matches!(m.screen, Screen::SignedIn(_)));
        m.update(Event::Polled(Err(Error::Io("reset".into()))));
        assert!(matches!(m.screen, Screen::NotRunning(_)));
        assert_eq!(m.update(Event::TryAgain), vec![Effect::StartAgent]);
        // A join whose agent went away.
        let mut m = on_join_form();
        form(&mut m).join = "cloud.shadows.ch/K7M4-Q9TX".into();
        m.update(Event::SubmitJoin);
        m.update(Event::Joined(Err(Error::NotRunning("gone".into()))));
        assert!(matches!(m.screen, Screen::NotRunning(_)));
    }

    #[test]
    fn sign_out_offline_offers_this_computer_only() {
        let mut m = signed_in();
        assert_eq!(m.update(Event::SignOut), vec![Effect::Logout { local: false }]);
        assert!(m.update(Event::SignOut).is_empty());
        m.update(Event::SignedOut(Err(refused(codes::OFFLINE, "can't reach the server to sign this device out"))));
        assert!(m.offer_local_sign_out && m.sign_out_error.is_some());
        assert_eq!(m.update(Event::SignOutHere), vec![Effect::Logout { local: true }]);
    }

    /// The footer's Sign out is there on every page of the signed-in window, and so are its problem and the offer.
    #[test]
    fn sign_out_works_from_every_page_and_its_problem_is_kept_there() {
        for tab in [Tab::Files, Tab::Versions, Tab::Trash, Tab::Security, Tab::Activity, Tab::Computers] {
            let mut m = signed_in();
            m.update(Event::Tab(tab));
            assert_eq!(m.tab(), Some(tab));
            assert_eq!(m.update(Event::SignOut), vec![Effect::Logout { local: false }], "{tab:?}");
            assert_eq!(m.busy, Some(Busy::SigningOut));
            assert!(m.update(Event::SignOut).is_empty(), "{tab:?}: once at a time");
            m.update(Event::SignedOut(Err(refused(codes::OFFLINE, "can't reach the server to sign this device out"))));
            assert_eq!(m.busy, None);
            assert!(m.sign_out_error.is_some() && m.offer_local_sign_out, "{tab:?}");
            assert_eq!(m.update(Event::SignOutHere), vec![Effect::Logout { local: true }], "{tab:?}");
            // A new try clears the problem.
            m.update(Event::SignedOut(Err(refused("other", "no"))));
            assert!(!m.offer_local_sign_out);
            m.update(Event::SignOut);
            assert!(m.sign_out_error.is_none(), "{tab:?}");
        }
        assert!(model().update(Event::SignOut).is_empty(), "not signed in");
    }

    #[test]
    fn a_tab_does_not_leave_the_security_page_while_it_works_or_shows_recovery_codes() {
        let mut m = signed_in();
        m.update(Event::Tab(Tab::Security));
        assert!(!m.tabs_locked());
        let Screen::Security(page) = &mut m.screen else { panic!() };
        page.busy = true;
        assert!(m.tabs_locked());
        assert!(m.update(Event::Tab(Tab::Trash)).is_empty());
        assert_eq!(m.tab(), Some(Tab::Security));
        let Screen::Security(page) = &mut m.screen else { panic!() };
        page.busy = false;
        page.step = SecurityStep::Codes { codes: vec!["AAAA-BBBB".into()], why: crate::security::CodesWhy::TurnedOn };
        assert!(m.tabs_locked());
        assert!(m.update(Event::Tab(Tab::Overview)).is_empty());
        assert_eq!(m.tab(), Some(Tab::Security));
        // Done with the codes: the page is free again.
        m.update(Event::Security(SecurityEvent::Home));
        assert!(!m.tabs_locked());
        m.update(Event::Tab(Tab::Overview));
        assert_eq!(m.tab(), Some(Tab::Overview));
    }

    /// Sign out and Quit would drop the page like a tab does, so they wait too while the page holds a one-time code or a
    /// call is under way.
    #[test]
    fn sign_out_and_quit_wait_while_the_tabs_are_locked() {
        let mut m = signed_in();
        m.update(Event::Tab(Tab::Security));
        let Screen::Security(page) = &mut m.screen else { panic!() };
        page.step = SecurityStep::Codes { codes: vec!["AAAA-BBBB".into()], why: crate::security::CodesWhy::TurnedOn };
        assert!(m.tabs_locked());
        for event in [Event::SignOut, Event::SignOutHere, Event::Quit] {
            assert!(m.update(event.clone()).is_empty(), "{event:?}");
        }
        assert_eq!(m.busy, None);
        // Done with the codes: both work again.
        m.update(Event::Security(SecurityEvent::Home));
        assert!(!m.tabs_locked());
        assert_eq!(m.update(Event::SignOut), vec![Effect::Logout { local: false }]);
        let mut m = signed_in();
        m.update(Event::Tab(Tab::Security));
        assert_eq!(m.update(Event::Quit), vec![Effect::QuitTray, Effect::QuitAgent]);
    }

    #[test]
    fn the_admin_check_is_forgotten_when_the_account_changes() {
        let mut m = signed_in_admin();
        assert!(m.is_admin());
        let other = Status { account: Some("ana".into()), ..status(true) };
        m.update(Event::Polled(Ok(other)));
        assert_eq!(m.access, Access::Unknown);
    }

    /// Quit stops the agent, then the app; the close button only hides the window when there is a tray.
    #[test]
    fn quit_stops_the_agent_and_close_hides_the_window() {
        let mut m = signed_in();
        assert!(m.update(Event::CloseRequested).is_empty());
        assert!(!m.window_visible);
        m.update(Event::Show);
        assert!(m.window_visible);
        assert_eq!(m.update(Event::Quit), vec![Effect::QuitTray, Effect::QuitAgent], "the tray process too, on Linux");
        assert!(m.update(Event::Quit).is_empty());
        // The agent stopping isn't taken for a problem.
        m.update(Event::Polled(Err(Error::NotRunning("gone".into()))));
        assert!(matches!(m.screen, Screen::SignedIn(_)));
        assert!(m.update(Event::Tick { now_ms: 9 }).is_empty());
        assert_eq!(m.update(Event::AgentQuit), vec![Effect::QuitApp]);
    }

    /// Review focus: no tray (Hyprland without waybar's tray, GNOME without the extension): closing the window ends
    /// the app (the agent keeps syncing), and the app's own start-at-login entry isn't written.
    #[test]
    fn without_a_tray_close_quits_the_app_and_it_doesnt_start_at_login() {
        let mut m = Model::new(defaults(), false, String::new());
        m.update(Event::Start);
        let effects = m.update(Event::Found(Ok(status(true))));
        assert!(effects.contains(&Effect::AppAutostart(false)), "{effects:?}");
        assert!(!effects.contains(&Effect::AppAutostart(true)));
        assert_eq!(m.update(Event::CloseRequested), vec![Effect::QuitApp]);
        assert!(m.window_visible);
    }

    /// Linux: the window is a process of its own (Wayland can't hide a window), so closing it ends it; the login entry
    /// is the tray process's, written whether or not a tray is there now; the tray process's Quit ends the window.
    #[test]
    fn on_linux_the_window_ends_when_closed_and_the_tray_process_has_the_login_entry() {
        let mut m = Model::new(defaults(), false, String::new());
        m.login_entry = true;
        m.update(Event::Start);
        let effects = m.update(Event::Found(Ok(status(true))));
        assert!(effects.contains(&Effect::AppAutostart(true)), "{effects:?}");
        assert_eq!(m.update(Event::CloseRequested), vec![Effect::QuitApp]);
        assert_eq!(m.update(Event::Exit), vec![Effect::QuitApp]);
        // Removed at sign-out only.
        let effects = m.update(Event::SignedOut(Ok(status(false))));
        assert!(effects.contains(&Effect::AppAutostart(false)), "{effects:?}");
    }

    #[test]
    fn the_apps_start_at_login_follows_the_agents_where_there_is_a_tray() {
        let on = status(true);
        assert_eq!(app_autostart_wanted(&on, true), Some(true));
        assert_eq!(app_autostart_wanted(&on, false), Some(false), "no tray: never");
        assert_eq!(app_autostart_wanted(&Status { autostart: Some(false), ..on.clone() }, true), Some(false));
        assert_eq!(app_autostart_wanted(&Status { autostart: None, ..on.clone() }, true), None, "the agent doesn't say: left alone");
        assert_eq!(app_autostart_wanted(&status(false), true), Some(false), "signed out");
        // At each start, matched once; again only when it changes.
        let mut m = model();
        m.update(Event::Start);
        assert_eq!(
            m.update(Event::Found(Ok(on.clone()))),
            vec![
                Effect::AppAutostart(true),
                Effect::Devices,
                Effect::Account { session: m.overview().unwrap().session },
                Effect::Usage(1),
                Effect::Recent { session: m.overview().unwrap().session },
                Effect::Conflicts { root: Some("/home/ana/Craft Cloud".into()), ticket: m.overview().unwrap().conflicts_ticket }
            ]
        );
        assert!(m.update(Event::Polled(Ok(on.clone()))).is_empty());
        assert_eq!(m.update(Event::Polled(Ok(Status { autostart: Some(false), ..on }))), vec![Effect::AppAutostart(false)]);
    }

    #[test]
    fn first_sentences_make_one_line() {
        assert_eq!(first_sentence("can't reach it. Try later"), "Can't reach it.");
        assert_eq!(first_sentence("no dot here"), "No dot here");
        assert_eq!(sentence(""), "");
    }

    /// A signed-in model whose admin check said admin.
    fn signed_in_admin() -> Model {
        let mut m = signed_in();
        m.overview_mut().unwrap().access = Access::Admin { two_factor: false };
        m.access = Access::Admin { two_factor: false };
        assert!(m.is_admin());
        m
    }

    #[test]
    fn the_tabs_list_the_pages_and_the_admin_pages_for_an_admin_only() {
        let member = [Tab::Overview, Tab::Files, Tab::Shared, Tab::Versions, Tab::Trash, Tab::Computers, Tab::Security, Tab::Activity];
        assert_eq!(signed_in().tabs(), member);
        let mut admin = member.to_vec();
        admin.push(Tab::Admin);
        assert_eq!(signed_in_admin().tabs(), admin);
        assert_eq!(model().tab(), None, "no tabs before sign-in");
        assert_eq!(signed_in().tab(), Some(Tab::Overview));
    }

    #[test]
    fn a_tab_asks_for_what_the_button_it_replaces_asked_for() {
        let cases = [
            (Tab::Files, Event::OpenFiles),
            (Tab::Versions, Event::OpenVersions),
            (Tab::Trash, Event::OpenTrash),
            (Tab::Security, Event::OpenSecurity),
            (Tab::Activity, Event::Audit(AuditMsg::OpenActivity)),
        ];
        for (tab, old) in cases {
            let (mut by_tab, mut by_button) = (signed_in(), signed_in());
            let (got, want) = (by_tab.update(Event::Tab(tab)), by_button.update(old));
            if tab == Tab::Activity {
                // The tickets of the pages' answers count up over the whole run.
                assert!(matches!(got.as_slice(), [Effect::AuditPage { own: true, .. }]) && got.len() == want.len(), "{got:?}");
            } else if tab == Tab::Files {
                // The conflict copies' ticket is a fresh number, too.
                assert!(matches!(got.as_slice(), [Effect::Recents { .. }, Effect::Conflicts { .. }]) && got.len() == want.len(), "{got:?}");
            } else {
                assert_eq!(got, want, "{tab:?}");
            }
            assert_eq!(by_tab.tab(), Some(tab));
            assert_eq!(std::mem::discriminant(&by_tab.screen), std::mem::discriminant(&by_button.screen), "{tab:?}");
        }
        let (mut by_tab, mut by_button) = (signed_in_admin(), signed_in_admin());
        let effects = by_tab.update(Event::Tab(Tab::Admin));
        assert!(!effects.is_empty());
        assert_eq!(effects.len(), by_button.update(Event::Admin(AdminEvent::Open)).len());
        assert_eq!(by_tab.tab(), Some(Tab::Admin));
    }

    #[test]
    fn a_member_has_no_admin_tab_and_the_event_does_nothing() {
        let mut m = signed_in();
        assert!(m.update(Event::Tab(Tab::Admin)).is_empty());
        assert!(matches!(m.screen, Screen::SignedIn(_)));
        // Not from another page either.
        m.update(Event::Tab(Tab::Trash));
        assert!(m.update(Event::Tab(Tab::Admin)).is_empty());
        assert!(matches!(m.screen, Screen::Trash(_)));
    }

    #[test]
    fn the_overview_and_computers_tabs_swap_the_page_without_asking_for_anything() {
        let mut m = signed_in();
        assert!(m.update(Event::Tab(Tab::Computers)).is_empty());
        assert!(matches!(&m.screen, Screen::SignedIn(o) if o.page == OverviewPage::Computers));
        assert_eq!(m.tab(), Some(Tab::Computers));
        assert!(m.update(Event::Tab(Tab::Computers)).is_empty(), "the tab shown does nothing");
        assert!(m.update(Event::Tab(Tab::Overview)).is_empty());
        assert!(matches!(&m.screen, Screen::SignedIn(o) if o.page == OverviewPage::Home));
        // The computers page keeps its events: Add a computer and back.
        m.update(Event::Tab(Tab::Computers));
        assert_eq!(m.update(Event::AddComputer), vec![Effect::Pairing]);
        assert_eq!(m.tab(), Some(Tab::Computers));
        assert!(m.update(Event::Tab(Tab::Computers)).is_empty(), "the tab shown does nothing, the code page and its wait stay");
        assert!(matches!(m.screen, Screen::AddingComputer(_)));
        assert_eq!(m.update(Event::Tab(Tab::Overview)), vec![Effect::Devices], "another tab leaves it");
        assert!(matches!(&m.screen, Screen::SignedIn(o) if o.page == OverviewPage::Home));
    }

    #[test]
    fn a_tab_leaves_the_page_the_way_its_back_button_did() {
        // From trash: the answers still on their way are dropped, and the overview asks again for what it shows.
        let mut m = signed_in();
        m.update(Event::Tab(Tab::Trash));
        let ticket = m.trash_ticket;
        let effects = m.update(Event::Tab(Tab::Overview));
        assert!(matches!(m.screen, Screen::SignedIn(_)));
        assert!(
            matches!(
                effects.as_slice(),
                [Effect::Devices, Effect::Account { .. }, Effect::Usage(_), Effect::Recent { .. }, Effect::Conflicts { .. }]
            ),
            "{effects:?}"
        );
        assert_eq!(m.trash_ticket, ticket + 1);
        // From the security page to trash: the page's secrets go and the trash is asked for.
        m.update(Event::Tab(Tab::Security));
        let effects = m.update(Event::Tab(Tab::Trash));
        assert!(matches!(m.screen, Screen::Trash(_)));
        assert_eq!(effects.first(), Some(&Effect::Devices));
        assert!(effects.iter().any(|e| matches!(e, Effect::Trash { .. })), "{effects:?}");
        // From versions to the activity page.
        m.update(Event::Tab(Tab::Versions));
        let effects = m.update(Event::Tab(Tab::Activity));
        assert!(matches!(m.screen, Screen::Activity(_)));
        assert!(effects.iter().any(|e| matches!(e, Effect::AuditPage { own: true, .. })), "{effects:?}");
        // A wait for a computer's code is cancelled by leaving it.
        let mut m = signed_in();
        m.update(Event::AddComputer);
        let info = PairingInfo { id: Uuid::nil(), join: "x".into(), link: "x".into(), expires_ms: 1 };
        m.update(Event::PairingMade(Ok(info)));
        let effects = m.update(Event::Tab(Tab::Security));
        assert!(effects.contains(&Effect::CancelPairing(Uuid::nil())), "{effects:?}");
        assert!(matches!(m.screen, Screen::Security(_)));
    }

    #[test]
    fn the_admin_tab_is_still_there_from_trash_and_versions() {
        let mut m = signed_in_admin();
        m.update(Event::Tab(Tab::Trash));
        assert!(m.is_admin());
        assert!(m.tabs().contains(&Tab::Admin));
        let effects = m.update(Event::Tab(Tab::Admin));
        assert!(matches!(m.screen, Screen::Admin(_)), "{:?}", m.screen);
        assert!(!effects.is_empty());
        m.update(Event::Tab(Tab::Versions));
        assert!(m.tabs().contains(&Tab::Admin));
        // Signed out, nothing is remembered for the next account.
        m.update(Event::Polled(Ok(status(false))));
        assert_eq!(m.access, Access::Unknown);
    }
    fn trash_item(n: u128, name: &str, is_dir: bool, path: Option<&str>) -> craft_cloud_app::TrashItem {
        craft_cloud_app::TrashItem {
            id: NodeId::from_uuid(Uuid::from_u128(n)),
            parent: NodeId::from_uuid(Uuid::nil()),
            name: name.into(),
            is_dir,
            size: Some(10),
            deleted_ms: 0,
            purge_after_ms: 0,
            path: path.map(str::to_owned),
        }
    }

    fn computer(name: &str, current: bool) -> DeviceInfo {
        DeviceInfo {
            id: DeviceId::generate(),
            name: name.into(),
            created_ms: 1_790_000_000_000,
            last_used_ms: 1_790_000_000_000,
            revoked: false,
            current,
            client_kind: "agent".into(),
            platform: "Windows".into(),
            app_version: "0.1.0".into(),
            revoked_ms: None,
            revoke_reason: None,
        }
    }

    /// A signed-in model that knows two computers: this one and another.
    fn with_two_computers() -> (Model, DeviceInfo, DeviceInfo) {
        let mut m = signed_in();
        let (this, other) = (computer("lptp1", true), computer("BEAST", false));
        m.update(Event::Devices(Ok(vec![this.clone(), other.clone()])));
        (m, this, other)
    }

    fn renaming(m: &Model) -> Option<Rename> {
        m.overview().and_then(|o| o.renaming.clone())
    }

    fn type_name(m: &mut Model, name: &str) {
        if let Some(r) = m.overview_mut().and_then(|o| o.renaming.as_mut()) {
            r.name = name.to_owned();
        }
    }

    #[test]
    fn the_trash_list_is_filtered_by_kind_and_searched_by_name_or_path_in_any_case() {
        let mut m = model();
        let items = vec![
            trash_item(1, "Sketch.PSD", false, Some("art/Sketch.PSD")),
            trash_item(2, "Old exports", true, None),
            trash_item(3, "notes.txt", false, Some("Projects/notes.txt")),
        ];
        m.screen = Screen::Trash(Trash { page: Some(TrashResponse { items, next: None }), ..Trash::new() });
        let names = |m: &Model| match &m.screen {
            Screen::Trash(t) => t.visible().iter().map(|i| i.name.clone()).collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        assert_eq!(names(&m), ["Sketch.PSD", "Old exports", "notes.txt"]);
        let Screen::Trash(t) = &m.screen else { panic!("trash") };
        assert_eq!(t.counts(), (3, 1, 2));
        m.update(Event::TrashKind(TrashKind::Files));
        assert_eq!(names(&m), ["Sketch.PSD", "notes.txt"]);
        m.update(Event::TrashKind(TrashKind::Folders));
        assert_eq!(names(&m), ["Old exports"]);
        // The search looks at the path too, ignores the case and the spaces around, and works with the kind.
        m.update(Event::TrashKind(TrashKind::All));
        m.update(Event::TrashSearch("  projects/ ".into()));
        assert_eq!(names(&m), ["notes.txt"]);
        m.update(Event::TrashSearch("SKETCH".into()));
        assert_eq!(names(&m), ["Sketch.PSD"]);
        m.update(Event::TrashKind(TrashKind::Folders));
        assert!(names(&m).is_empty(), "a folder that doesn't match is hidden");
        m.update(Event::TrashSearch(String::new()));
        m.update(Event::TrashKind(TrashKind::All));
        assert_eq!(names(&m).len(), 3);
        // The counts don't follow the search: they say what is loaded.
        m.update(Event::TrashSearch("zzz".into()));
        let Screen::Trash(t) = &m.screen else { panic!("trash") };
        assert_eq!((t.visible().len(), t.counts()), (0, (3, 1, 2)));
    }

    #[test]
    fn the_filter_does_nothing_off_the_trash_page_and_survives_a_reload() {
        let mut m = signed_in();
        assert!(m.update(Event::TrashKind(TrashKind::Files)).is_empty());
        assert!(m.update(Event::TrashSearch("x".into())).is_empty());
        m.update(Event::Tab(Tab::Trash));
        m.update(Event::TrashSearch("x".into()));
        m.update(Event::LoadTrash);
        assert!(matches!(&m.screen, Screen::Trash(t) if t.search == "x"), "a refresh keeps what was typed");
    }

    #[test]
    fn cancelling_a_removal_closes_the_confirmation_and_sends_nothing() {
        let mut m = model();
        let page = trash_page();
        let id = page.items[0].id;
        m.screen = Screen::Trash(Trash { page: Some(page), ..Trash::new() });
        m.update(Event::ConfirmRemoveTrash(id));
        assert!(matches!(&m.screen, Screen::Trash(t) if t.confirm == Some(id)));
        assert!(m.update(Event::CancelRemoveTrash).is_empty());
        assert!(matches!(&m.screen, Screen::Trash(t) if t.confirm.is_none()));
        // With nothing to confirm, the remove button does nothing.
        assert!(m.update(Event::RemoveTrash).is_empty());
    }

    #[test]
    fn the_devices_answer_teaches_the_names_wherever_the_page_is() {
        let mut m = signed_in();
        let device = DeviceInfo {
            id: DeviceId::generate(),
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
        };
        // On a page without the signed-in view (versions) the names are kept all the same.
        m.update(Event::Tab(Tab::Versions));
        assert!(m.overview().is_none());
        m.update(Event::Devices(Ok(vec![device.clone()])));
        assert_eq!(m.names.computer(device.id).text, "BEAST");
        // The computer leaves the list: its name stays for the older entries.
        m.update(Event::Devices(Ok(Vec::new())));
        assert_eq!(m.names.computer(device.id).text, "BEAST");
        assert!(!m.names.computer(DeviceId::generate()).known);
    }

    #[test]
    fn the_account_answer_teaches_the_accounts_own_name() {
        let mut m = signed_in();
        let session = m.overview().unwrap().session;
        let id = Uuid::from_u128(77);
        m.update(Event::Admin(AdminEvent::Account {
            session,
            result: Ok(craft_cloud_proto::api::AccountInfo {
                user_id: id,
                username: "ana".into(),
                role: "member".into(),
                created_ms: 0,
                password_changed_ms: None,
                two_factor_enabled: false,
            }),
        }));
        assert_eq!(m.names.account(id).text, "ana");
    }

    #[test]
    fn the_recent_activity_loads_with_the_overview_and_keeps_the_newest_five() {
        let mut m = signed_in();
        let o = m.overview().unwrap();
        assert_eq!(o.recent, Recent::Loading);
        let session = o.session;
        let events: Vec<_> = (0..8).map(|n| crate::agent::fake::FakeAgent::sample_event(n, "auth.login")).collect();
        // An answer for another sign-in is ignored.
        m.update(Event::Recent { session: session + 1, result: Ok(AuditPage { events: events.clone(), next: None }) });
        assert_eq!(m.overview().unwrap().recent, Recent::Loading);
        m.update(Event::Recent { session, result: Ok(AuditPage { events: events.clone(), next: Some("more".into()) }) });
        assert!(matches!(&m.overview().unwrap().recent, Recent::Loaded(list) if list.len() == 5 && list[0].id == events[0].id));
        m.update(Event::Recent { session, result: Err(Error::Io("down".into())) });
        assert!(matches!(&m.overview().unwrap().recent, Recent::Failed(_)));
        // A fresh overview asks again.
        m.update(Event::Tab(Tab::Trash));
        let effects = m.update(Event::Tab(Tab::Overview));
        assert!(effects.iter().any(|e| matches!(e, Effect::Recent { .. })), "{effects:?}");
        assert_eq!(m.overview().unwrap().recent, Recent::Loading);
    }

    fn recent_file(app: &str, path: &str, title: &str, opened_ms: i64, place: Place) -> RecentFile {
        RecentFile {
            app: app.into(),
            entry: craft_cloud_app::RecentEntry {
                node: None,
                path: path.into(),
                title: title.into(),
                opened_ms,
                saved_ms: None,
                machine: "BEAST".into(),
            },
            place,
        }
    }

    fn files_list() -> Vec<RecentFile> {
        vec![
            recent_file("wordcraft", "Letters/letter.docx", "Letter", 30, Place::Here),
            recent_file("photocraft", "art/cover.psd", "cover.psd", 20, Place::NotYet),
            recent_file("wordcraft", "memo.docx", "Memo", 10, Place::Here),
        ]
    }

    /// The Files page after its list came.
    fn files_loaded() -> Model {
        let mut m = signed_in();
        let effects = m.update(Event::Tab(Tab::Files));
        let [Effect::Recents { ticket, .. }, Effect::Conflicts { .. }] = effects.as_slice() else { panic!("{effects:?}") };
        assert!(m.update(Event::Recents { ticket: *ticket, result: Ok(files_list()) }).is_empty());
        m
    }

    fn conflict_copy(path: &str, original: Option<&str>) -> craft_cloud_app::ConflictCopy {
        craft_cloud_app::ConflictCopy {
            path: path.into(),
            original_path: original.map(Into::into),
            device: Some("BEAST".into()),
            at: Some("2026-10-12 14:03".into()),
            at_utc: false,
            at_ms: None,
            size: 2048,
            mtime_ms: 1_000,
        }
    }

    /// The Files page with its conflict copies answered: the list, placed by `place`.
    fn conflicts_loaded(copies: Vec<craft_cloud_app::ConflictCopy>, truncated: bool, place: impl Fn(&str) -> Place) -> Model {
        let mut m = files_loaded();
        let ticket = files_page(&m).conflicts.ticket;
        let list = crate::files::conflicts(craft_cloud_app::ConflictsResponse { copies, truncated }, place);
        assert!(m.update(Event::Conflicts { ticket, result: Ok(list) }).is_empty());
        m
    }

    /// The folder sizes asked for on the home page are answered while the overview is under another screen: the answer is
    /// taken, and a spinner that was left on is off when the overview comes back.
    #[test]
    fn the_folder_sizes_answer_reaches_an_overview_under_another_screen() {
        let data = craft_cloud_app::FolderUsageResponse { total_bytes: 5, total_files: 1, ..Default::default() };
        let mut m = signed_in();
        m.overview_mut().unwrap().usage_loading = false;
        assert!(m.update(Event::ToggleFolders).iter().all(|e| matches!(e, Effect::FolderUsage { .. })));
        let ticket = m.overview().map(|o| o.folders.ticket).unwrap_or(0);
        m.update(Event::Tab(Tab::Security));
        assert!(matches!(m.screen, Screen::Security(_)));
        m.update(Event::FolderUsage { ticket, result: Ok(data.clone()) });
        let folders = &m.overview().unwrap().folders;
        assert!(!folders.loading && folders.data.as_ref() == Some(&data), "the answer is taken under the Security page");
        m.update(Event::FolderUsage { ticket: ticket + 1, result: Err(Error::NotRunning("stale".into())) });
        assert!(m.overview().unwrap().folders.error.is_none(), "another ticket's answer is still ignored");
        // A request still under way when the screen over the overview is left no longer spins.
        m.overview_mut().unwrap().folders.loading = true;
        m.update(Event::Security(SecurityEvent::Close));
        assert!(matches!(m.screen, Screen::SignedIn(_)));
        assert!(!m.overview().unwrap().folders.loading);
    }

    #[test]
    fn conflict_copies_load_with_the_recents_and_a_stale_answer_is_ignored() {
        let mut m = files_loaded();
        assert!(files_page(&m).conflicts.loading && files_page(&m).conflicts.list.is_none());
        m.update(Event::Conflicts { ticket: 1, result: Ok(Default::default()) });
        assert!(files_page(&m).conflicts.loading, "another ticket's answer is ignored");
        let ticket = files_page(&m).conflicts.ticket;
        m.update(Event::Conflicts { ticket, result: Err(Error::NotRunning("gone".into())) });
        let state = &files_page(&m).conflicts;
        assert!(!state.loading && state.list.is_none() && state.error.is_some());
        assert_eq!(files_page(&m).view, FilesView::Recent);
        m.update(Event::FilesView(FilesView::Conflicts));
        assert_eq!(files_page(&m).view, FilesView::Conflicts);
    }

    #[test]
    fn a_conflict_copy_opens_only_when_it_is_here_and_the_original_is_shown_by_its_folder() {
        let place = |path: &str| {
            if path.starts_with("gone/") {
                Place::NotYet
            } else if path.ends_with(".exe") {
                Place::NotOpenable
            } else {
                Place::Here
            }
        };
        let copies = vec![
            conflict_copy("Letters/a (conflict).docx", Some("Letters/a.docx")),
            conflict_copy("top (conflict).txt", Some("top.txt")),
            conflict_copy("gone/b (conflict).txt", Some("gone/b.txt")),
            conflict_copy("odd.txt", None),
            conflict_copy("tool (conflict).exe", Some("tool.exe")),
            conflict_copy("Photos.2024/a (conflict).png", Some("Photos.2024/a.png")),
            conflict_copy("Plain folder/in.sh (conflict).txt", Some("Plain folder/in.sh")),
            conflict_copy("Tool.app/readme (conflict).txt", Some("Tool.app/readme.txt")),
            conflict_copy("up/../x (conflict).txt", Some("up/../x.txt")),
        ];
        let mut m = conflicts_loaded(copies, false, place);
        let root = std::path::Path::new("/home/ana/Craft Cloud");
        let open = m.update(Event::OpenConflictCopy("Letters/a (conflict).docx".into()));
        // The disk is looked at again at the click, and what that finds is what is opened (the model never opens the joined path).
        assert_eq!(open, vec![Effect::CheckFile { root: "/home/ana/Craft Cloud".into(), path: "Letters/a (conflict).docx".into() }]);
        let checked = root.join("Letters").join("a (conflict).docx").to_string_lossy().into_owned();
        assert_eq!(m.update(Event::FileChecked(Ok(checked.clone()))), vec![Effect::OpenFile(checked)]);
        let show = m.update(Event::ShowConflictOriginal("Letters/a (conflict).docx".into()));
        assert_eq!(show, vec![Effect::OpenFolder(root.join("Letters").to_string_lossy().into_owned())]);
        let top = m.update(Event::ShowConflictOriginal("top (conflict).txt".into()));
        assert_eq!(top, vec![Effect::OpenFolder("/home/ana/Craft Cloud".into())], "an original at the top is in the synced folder itself");
        for (event, why) in [
            (Event::OpenConflictCopy("gone/b (conflict).txt".into()), "Not on this computer yet"),
            (Event::ShowConflictOriginal("gone/b (conflict).txt".into()), "Not on this computer yet"),
            (Event::OpenConflictCopy("tool (conflict).exe".into()), "Open it from its folder"),
            (Event::OpenConflictCopy("Tool.app/readme (conflict).txt".into()), "Open it from its folder"),
            (Event::ShowConflictOriginal("odd.txt".into()), "doesn't say which file"),
        ] {
            assert!(m.update(event.clone()).is_empty(), "{event:?}");
            assert!(
                files_page(&m).conflicts.open_error.as_deref().is_some_and(|e| e.contains(why)),
                "{event:?}: {:?}",
                files_page(&m).conflicts.open_error
            );
        }
        assert!(m.update(Event::OpenConflictCopy("not/in/the/list.txt".into())).is_empty());
        // The second look finding the copy gone, or something else, is said on the conflict copies' page.
        m.update(Event::FilesView(FilesView::Conflicts));
        assert!(m.update(Event::FileChecked(Err(Place::NotYet))).is_empty());
        assert_eq!(files_page(&m).conflicts.open_error.as_deref(), Some("Can't open this file: Not on this computer yet."));
        assert!(files_page(&m).open_error.is_none());
    }

    /// Show original takes the original's folder with the checks of a path only: a folder called `Photos.2024` or a file that
    /// is a program is fine to show, a bundle or an escape is not.
    #[test]
    fn show_original_checks_the_folder_as_a_path_not_as_a_program() {
        let place = |path: &str| if path.ends_with(".sh") { Place::NotOpenable } else { Place::Here };
        let copies = vec![
            conflict_copy("Photos.2024/a (conflict).png", Some("Photos.2024/a.png")),
            conflict_copy("Plain folder/in.sh (conflict).txt", Some("Plain folder/in.sh")),
            conflict_copy("Tool.app/readme (conflict).txt", Some("Tool.app/readme.txt")),
            conflict_copy("up/../x (conflict).txt", Some("up/../x.txt")),
            conflict_copy("Noext/README (conflict)", Some("Noext/README")),
        ];
        let mut m = conflicts_loaded(copies, false, place);
        let root = std::path::Path::new("/home/ana/Craft Cloud");
        let shown = |folder: &str| vec![Effect::OpenFolder(root.join(folder).to_string_lossy().into_owned())];
        assert_eq!(m.update(Event::ShowConflictOriginal("Photos.2024/a (conflict).png".into())), shown("Photos.2024"));
        assert_eq!(m.update(Event::ShowConflictOriginal("Plain folder/in.sh (conflict).txt".into())), shown("Plain folder"));
        assert_eq!(m.update(Event::ShowConflictOriginal("Noext/README (conflict)".into())), shown("Noext"));
        for (copy, why) in [("Tool.app/readme (conflict).txt", "Open it from its folder"), ("up/../x (conflict).txt", "Outside the synced folder")] {
            assert!(m.update(Event::ShowConflictOriginal(copy.into())).is_empty(), "{copy}");
            let error = files_page(&m).conflicts.open_error.clone();
            assert!(error.as_deref().is_some_and(|e| e.starts_with("Can't show the original") && e.contains(why)), "{copy}: {error:?}");
        }
    }

    #[test]
    fn the_overview_counts_conflict_copies_and_its_link_opens_them() {
        let mut m = signed_in();
        let ticket = m.overview().map(|o| o.conflicts_ticket).unwrap_or(0);
        let list = crate::files::conflicts(
            craft_cloud_app::ConflictsResponse { copies: vec![conflict_copy("a (c).txt", Some("a.txt")); 3], truncated: true },
            |_| Place::Here,
        );
        m.update(Event::Conflicts { ticket: ticket + 1, result: Ok(list.clone()) });
        assert_eq!(m.overview().and_then(|o| o.conflicts), None, "a stale answer is ignored");
        m.update(Event::Conflicts { ticket, result: Ok(list) });
        assert_eq!(m.overview().and_then(|o| o.conflicts), Some(ConflictSummary { count: 3, truncated: true }));
        let effects = m.update(Event::OpenConflicts);
        assert!(matches!(effects.as_slice(), [Effect::Recents { .. }, Effect::Conflicts { .. }]), "{effects:?}");
        assert_eq!(files_page(&m).view, FilesView::Conflicts);
        // Refreshing the overview asks again; an error leaves no tag.
        let mut m = signed_in();
        m.overview_mut().unwrap().usage_loading = false;
        let refresh = m.update(Event::RefreshUsage);
        let Some(Effect::Conflicts { ticket, .. }) = refresh.iter().find(|e| matches!(e, Effect::Conflicts { .. })) else { panic!("{refresh:?}") };
        m.update(Event::Conflicts { ticket: *ticket, result: Err(Error::NotRunning("gone".into())) });
        assert_eq!(m.overview().and_then(|o| o.conflicts), None);
    }

    #[test]
    fn the_folder_sizes_load_when_the_section_opens() {
        let mut m = signed_in();
        assert!(m.update(Event::ToggleFolders).iter().all(|e| matches!(e, Effect::FolderUsage { .. })));
        let ticket = m.overview().map(|o| o.folders.ticket).unwrap_or(0);
        assert!(m.overview().is_some_and(|o| o.folders.open && o.folders.loading));
        let data = craft_cloud_app::FolderUsageResponse { total_bytes: 5, total_files: 1, ..Default::default() };
        m.update(Event::FolderUsage { ticket: ticket + 1, result: Ok(data.clone()) });
        assert!(m.overview().is_some_and(|o| o.folders.data.is_none()), "a stale answer is ignored");
        m.update(Event::FolderUsage { ticket, result: Ok(data.clone()) });
        assert_eq!(m.overview().and_then(|o| o.folders.data.clone()), Some(data));
        // Closed and opened again, it has its numbers and asks for nothing; Refresh asks again while it is open.
        assert!(m.update(Event::ToggleFolders).is_empty());
        assert!(m.update(Event::ToggleFolders).is_empty());
        m.overview_mut().unwrap().usage_loading = false;
        let refresh = m.update(Event::RefreshUsage);
        assert!(refresh.iter().any(|e| matches!(e, Effect::FolderUsage { .. })), "{refresh:?}");
        m.update(Event::FolderUsage { ticket: m.overview().map(|o| o.folders.ticket).unwrap_or(0), result: Err(Error::NotRunning("gone".into())) });
        assert!(m.overview().is_some_and(|o| o.folders.error.is_some() && !o.folders.loading));
    }

    fn files_page(m: &Model) -> &Files {
        let Screen::Files(files) = &m.screen else { panic!("{:?}", m.screen) };
        files
    }

    #[test]
    fn the_files_page_asks_for_the_recent_files_when_it_opens_and_not_on_the_overview() {
        let mut m = model();
        m.update(Event::Start);
        let overview = m.update(Event::Found(Ok(status(true))));
        assert!(!overview.iter().any(|e| matches!(e, Effect::Recents { .. })), "{overview:?}");
        let effects = m.update(Event::Tab(Tab::Files));
        assert!(
            matches!(effects.as_slice(), [Effect::Recents { root: Some(root), ticket: 1 }, Effect::Conflicts { root: Some(same), .. }] if root == same && root == "/home/ana/Craft Cloud"),
            "{effects:?}"
        );
        assert!(files_page(&m).loading && files_page(&m).list.is_none());
        assert_eq!(m.tab(), Some(Tab::Files));
    }

    /// The status poll every two seconds leaves the Files page where it is (it used to put the overview back).
    #[test]
    fn a_status_poll_leaves_the_files_page_where_it_is() {
        let mut m = signed_in();
        m.update(Event::Tab(Tab::Files));
        m.update(Event::Recents { ticket: 1, result: Ok(files_list()) });
        assert!(m.update(Event::Polled(Ok(status(true)))).is_empty());
        assert_eq!(m.tab(), Some(Tab::Files));
        assert!(files_page(&m).list.is_some(), "the loaded list is still there");
    }

    #[test]
    fn recent_files_load_once_at_a_time_and_stale_answers_are_ignored() {
        let mut m = signed_in();
        m.update(Event::Tab(Tab::Files));
        assert!(m.update(Event::LoadFiles).is_empty(), "one at a time");
        assert!(m.update(Event::Recents { ticket: 9, result: Ok(files_list()) }).is_empty());
        assert!(files_page(&m).loading && files_page(&m).list.is_none(), "another ticket's answer is ignored");
        m.update(Event::Recents { ticket: 1, result: Ok(files_list()) });
        assert!(!files_page(&m).loading);
        assert_eq!(files_page(&m).list.as_ref().map(Vec::len), Some(3));
        // Refresh asks again, with a new ticket.
        let again = m.update(Event::LoadFiles);
        assert!(matches!(again.as_slice(), [Effect::Recents { ticket: 2, .. }, Effect::Conflicts { .. }]), "{again:?}");
        // Leaving the page drops an answer that is still on its way.
        m.update(Event::Tab(Tab::Overview));
        assert!(m.update(Event::Recents { ticket: 2, result: Ok(Vec::new()) }).is_empty());
        assert!(matches!(m.screen, Screen::SignedIn(_)));
    }

    #[test]
    fn recent_files_that_cant_be_had_are_a_problem_and_an_empty_list_is_not() {
        let mut m = signed_in();
        m.update(Event::Tab(Tab::Files));
        m.update(Event::Recents { ticket: 1, result: Err(Error::NotRunning("connection refused".into())) });
        let files = files_page(&m);
        assert!(!files.loading && files.list.is_none() && files.error.as_deref().is_some_and(|e| e.contains("connection refused")), "{files:?}");
        m.update(Event::LoadFiles);
        assert!(files_page(&m).error.is_none(), "asking again clears it");
        m.update(Event::Recents { ticket: 2, result: Ok(Vec::new()) });
        let files = files_page(&m);
        assert!(files.error.is_none() && files.list.as_deref() == Some(&[][..]));
    }

    #[test]
    fn the_files_filter_and_search_narrow_the_loaded_list_without_asking_again() {
        let mut m = files_loaded();
        assert_eq!(files_page(&m).counts(), (3, vec![("photocraft".into(), 1), ("wordcraft".into(), 2)]));
        assert!(m.update(Event::FilesApp(Some("wordcraft".into()))).is_empty());
        assert_eq!(files_page(&m).visible().len(), 2);
        assert!(m.update(Event::FilesSearch("LETTERS/".into())).is_empty());
        let names: Vec<&str> = files_page(&m).visible().iter().map(|f| f.name()).collect();
        assert_eq!(names, ["Letter"], "the path is searched too");
        m.update(Event::FilesApp(None));
        m.update(Event::FilesSearch("cover".into()));
        assert_eq!(files_page(&m).visible().len(), 1);
        // A refresh that no longer has the filtered app's files shows all again.
        m.update(Event::FilesSearch(String::new()));
        m.update(Event::FilesApp(Some("photocraft".into())));
        m.update(Event::LoadFiles);
        m.update(Event::Recents { ticket: 2, result: Ok(vec![recent_file("wordcraft", "memo.docx", "Memo", 1, Place::Here)]) });
        assert_eq!(files_page(&m).app, None);
        assert_eq!(files_page(&m).visible().len(), 1);
    }

    #[test]
    fn open_asks_the_os_for_a_file_that_is_here_inside_the_synced_folder() {
        let mut m = files_loaded();
        let effects = m.update(Event::OpenRecent { app: "wordcraft".into(), path: "Letters/letter.docx".into() });
        assert_eq!(effects, vec![Effect::CheckFile { root: "/home/ana/Craft Cloud".into(), path: "Letters/letter.docx".into() }]);
        assert!(files_page(&m).open_error.is_none());
        // The disk is looked at again at the click, and the path that check gives is the one opened.
        let checked = "/home/ana/Craft Cloud/Letters/letter.docx".to_owned();
        assert_eq!(m.update(Event::FileChecked(Ok(checked.clone()))), vec![Effect::OpenFile(checked)]);
        assert!(files_page(&m).open_error.is_none());
        // Whatever the OS says of it comes back.
        assert!(m.update(Event::FileOpened(Ok(()))).is_empty());
        assert!(files_page(&m).open_error.is_none());
        assert!(m.update(Event::FileOpened(Err("xdg-open didn't start: not found".into()))).is_empty());
        assert_eq!(files_page(&m).open_error.as_deref(), Some("Couldn't open the file: xdg-open didn't start: not found"));
    }

    #[test]
    fn open_does_not_start_a_file_the_second_look_refuses() {
        for (place, why) in [
            (Place::NotYet, "Not on this computer yet"),
            (Place::Outside, "Outside the synced folder"),
            (Place::NotOpenable, "Open it from its folder"),
        ] {
            let mut m = files_loaded();
            m.update(Event::OpenRecent { app: "wordcraft".into(), path: "Letters/letter.docx".into() });
            assert!(m.update(Event::FileChecked(Err(place))).is_empty(), "{place:?}");
            assert_eq!(files_page(&m).open_error.as_deref(), Some(format!("Can't open this file: {why}.").as_str()));
        }
    }

    #[test]
    fn show_in_folder_opens_the_folder_of_a_file_that_is_here() {
        let mut m = files_loaded();
        let folder = Path::new("/home/ana/Craft Cloud").join("Letters").to_string_lossy().into_owned();
        assert_eq!(m.update(Event::ShowRecent { app: "wordcraft".into(), path: "Letters/letter.docx".into() }), vec![Effect::OpenFolder(folder)]);
        assert_eq!(
            m.update(Event::ShowRecent { app: "wordcraft".into(), path: "memo.docx".into() }),
            vec![Effect::OpenFolder("/home/ana/Craft Cloud".into())],
            "a file at the top of the synced folder shows that folder"
        );
        // Not here: nothing to show.
        assert!(m.update(Event::ShowRecent { app: "photocraft".into(), path: "art/cover.psd".into() }).is_empty());
        assert_eq!(files_page(&m).open_error.as_deref(), Some("Can't show this file: Not on this computer yet."));
        // A file the page won't open can still be shown, unless its folder is a bundle or leaves the synced folder.
        let mut m = signed_in();
        m.update(Event::Tab(Tab::Files));
        let list = vec![
            recent_file("wordcraft", "tools/setup.exe", "setup", 3, Place::NotOpenable),
            recent_file("wordcraft", "Tool.app/readme.txt", "readme", 2, Place::NotOpenable),
            recent_file("wordcraft", "../x.txt", "x", 1, Place::Here),
        ];
        m.update(Event::Recents { ticket: 1, result: Ok(list) });
        let shown = Path::new("/home/ana/Craft Cloud").join("tools").to_string_lossy().into_owned();
        assert_eq!(m.update(Event::ShowRecent { app: "wordcraft".into(), path: "tools/setup.exe".into() }), vec![Effect::OpenFolder(shown)]);
        assert!(m.update(Event::ShowRecent { app: "wordcraft".into(), path: "Tool.app/readme.txt".into() }).is_empty());
        assert!(m.update(Event::ShowRecent { app: "wordcraft".into(), path: "../x.txt".into() }).is_empty());
    }

    #[test]
    fn open_refuses_a_file_that_is_not_here_and_says_why() {
        let mut m = files_loaded();
        assert!(m.update(Event::OpenRecent { app: "photocraft".into(), path: "art/cover.psd".into() }).is_empty());
        assert_eq!(files_page(&m).open_error.as_deref(), Some("Can't open this file: Not on this computer yet."));
        // Nothing is asked of the OS for a file the list doesn't have.
        assert!(m.update(Event::OpenRecent { app: "wordcraft".into(), path: "art/cover.psd".into() }).is_empty());
        assert!(m.update(Event::OpenRecent { app: "wordcraft".into(), path: "/etc/passwd".into() }).is_empty());
    }

    /// A path from the network that gets past the list's placing (a list placed before the folder changed, or a bug) is
    /// checked again at Open: a root escape, an absolute path, a drive letter and a program are all refused.
    #[test]
    fn open_checks_the_path_again_and_refuses_a_root_escape() {
        for (path, why) in [
            ("../outside.docx", "Outside the synced folder"),
            ("a/../../outside.docx", "Outside the synced folder"),
            ("/etc/passwd", "Outside the synced folder"),
            ("C:/Windows/win.ini", "Outside the synced folder"),
            ("..\\outside.docx", "Outside the synced folder"),
            ("tools/setup.exe", "Open it from its folder"),
            ("tools/README", "Open it from its folder"),
            ("Tool.app/readme.txt", "Open it from its folder"),
        ] {
            let mut m = signed_in();
            m.update(Event::Tab(Tab::Files));
            m.update(Event::Recents { ticket: 1, result: Ok(vec![recent_file("wordcraft", path, "x", 1, Place::Here)]) });
            let effects = m.update(Event::OpenRecent { app: "wordcraft".into(), path: path.into() });
            assert!(effects.is_empty(), "{path}: {effects:?}");
            assert_eq!(files_page(&m).open_error.as_deref(), Some(format!("Can't open this file: {why}.").as_str()), "{path}");
        }
        // Placed outside, as a program or not here, a file is not opened whatever its path looks like.
        for place in [Place::Outside, Place::NotOpenable, Place::NotYet] {
            let mut m = signed_in();
            m.update(Event::Tab(Tab::Files));
            m.update(Event::Recents { ticket: 1, result: Ok(vec![recent_file("wordcraft", "ok.docx", "x", 1, place)]) });
            assert!(m.update(Event::OpenRecent { app: "wordcraft".into(), path: "ok.docx".into() }).is_empty(), "{place:?}");
            assert!(files_page(&m).open_error.is_some());
        }
        // No synced folder known: nothing to open it from.
        let mut m = signed_in();
        m.update(Event::Tab(Tab::Files));
        m.update(Event::Recents { ticket: 1, result: Ok(files_list()) });
        m.status = Some(Status { folder: None, ..status(true) });
        assert!(m.update(Event::OpenRecent { app: "wordcraft".into(), path: "memo.docx".into() }).is_empty());
        assert!(files_page(&m).open_error.is_some());
    }

    #[test]
    fn a_new_load_clears_the_open_problem_and_the_page_is_left_by_any_tab() {
        let mut m = files_loaded();
        m.update(Event::FileOpened(Err("no opener".into())));
        m.update(Event::LoadFiles);
        assert!(files_page(&m).open_error.is_none());
        let effects = m.update(Event::Tab(Tab::Computers));
        assert!(effects.contains(&Effect::Devices), "{effects:?}");
        assert_eq!(m.tab(), Some(Tab::Computers));
        assert!(m.update(Event::OpenRecent { app: "wordcraft".into(), path: "memo.docx".into() }).is_empty(), "not on the page any more");
    }

    #[test]
    fn a_rename_sends_the_trimmed_name_and_the_list_takes_the_answer() {
        let (mut m, this, other) = with_two_computers();
        assert!(m.update(Event::StartRename(other.id)).is_empty());
        assert_eq!(renaming(&m).map(|r| (r.id, r.name)), Some((other.id, "BEAST".to_owned())));
        type_name(&mut m, "  Studio PC \t");
        let effects = m.update(Event::SaveRename);
        assert_eq!(effects, vec![Effect::RenameDevice { id: other.id, name: "Studio PC".into(), ticket: 1 }]);
        assert!(renaming(&m).is_some_and(|r| r.busy));
        // Nothing goes twice while the answer is awaited.
        assert!(m.update(Event::SaveRename).is_empty());
        let renamed = DeviceInfo { name: "Studio PC".into(), ..other.clone() };
        // Another computer's name is not the agent's status: only the list changes.
        assert!(m.update(Event::Renamed(other.id, 1, Ok(renamed.clone()))).is_empty());
        assert_eq!(renaming(&m), None);
        let list = m.overview().and_then(|o| o.devices.clone()).unwrap_or_default();
        assert_eq!(list, vec![this, renamed], "the order stays, the row is the server's");
    }

    #[test]
    fn this_computer_can_be_renamed_too() {
        let (mut m, this, _) = with_two_computers();
        m.update(Event::StartRename(this.id));
        type_name(&mut m, "Office");
        assert_eq!(m.update(Event::SaveRename), vec![Effect::RenameDevice { id: this.id, name: "Office".into(), ticket: 1 }]);
    }

    #[test]
    fn renaming_this_computer_asks_for_the_status_again() {
        let (mut m, this, _) = with_two_computers();
        m.update(Event::StartRename(this.id));
        type_name(&mut m, "Office");
        m.update(Event::SaveRename);
        let renamed = DeviceInfo { name: "Office".into(), ..this.clone() };
        // The overview's title is the agent's status, which changed with the name.
        assert_eq!(m.update(Event::Renamed(this.id, 1, Ok(renamed.clone()))), vec![Effect::Status]);
        assert_eq!(m.overview().and_then(|o| o.devices.clone()).and_then(|d| d.first().map(|d| d.name.clone())).as_deref(), Some("Office"));
        // A refused rename changes nothing, so nothing is asked.
        m.update(Event::StartRename(this.id));
        type_name(&mut m, "Home");
        m.update(Event::SaveRename);
        assert!(m.update(Event::Renamed(this.id, 2, Err(refused("invalid", "no")))).is_empty());
    }

    #[test]
    fn a_name_the_server_would_refuse_is_not_sent() {
        let (mut m, _, other) = with_two_computers();
        m.update(Event::StartRename(other.id));
        for (typed, why) in [
            ("", "Type a name for this computer."),
            ("   ", "Type a name for this computer."),
            ("a\u{7}b", "A name can't hold control characters."),
            (&"x".repeat(MAX_DEVICE_NAME_BYTES + 1), "That name is too long. Use at most 128 bytes."),
        ] {
            type_name(&mut m, typed);
            assert!(m.update(Event::SaveRename).is_empty(), "{typed:?}");
            let rename = renaming(&m);
            assert_eq!(rename.as_ref().and_then(|r| r.error.as_deref()), Some(why), "{typed:?}");
            assert!(rename.is_some_and(|r| !r.busy));
        }
        // The longest name still goes (128 bytes, counted in bytes: "é" is two).
        type_name(&mut m, &"é".repeat(64));
        assert_eq!(m.update(Event::SaveRename).len(), 1);
    }

    #[test]
    fn saving_the_name_it_has_sends_nothing() {
        let (mut m, _, other) = with_two_computers();
        m.update(Event::StartRename(other.id));
        type_name(&mut m, " BEAST ");
        assert!(m.update(Event::SaveRename).is_empty());
        assert_eq!(renaming(&m), None);
    }

    #[test]
    fn a_refused_rename_keeps_the_field_with_the_reason_and_can_be_tried_again() {
        let (mut m, _, other) = with_two_computers();
        m.update(Event::StartRename(other.id));
        type_name(&mut m, "Studio PC");
        m.update(Event::SaveRename);
        let refused = Error::Refused { code: "not_found".into(), message: "no such computer in this cloud".into(), name_taken: None };
        m.update(Event::Renamed(other.id, 1, Err(refused)));
        let rename = renaming(&m);
        let seen = rename.as_ref().map(|r| (r.busy, r.error.as_deref(), r.name.as_str()));
        assert_eq!(seen, Some((false, Some("no such computer in this cloud"), "Studio PC")));
        let list = m.overview().and_then(|o| o.devices.clone()).unwrap_or_default();
        assert_eq!(list.get(1).map(|d| d.name.as_str()), Some("BEAST"));
        // Tried again, and the agent can't be reached: the same.
        assert_eq!(m.update(Event::SaveRename).len(), 1);
        m.update(Event::Renamed(other.id, 2, Err(Error::NotRunning("can't reach it".into()))));
        assert!(renaming(&m).is_some_and(|r| !r.busy && r.error.is_some()));
    }

    #[test]
    fn cancel_leaves_the_row_as_it_was_but_not_while_a_name_is_on_its_way() {
        let (mut m, _, other) = with_two_computers();
        m.update(Event::StartRename(other.id));
        type_name(&mut m, "Studio PC");
        m.update(Event::CancelRename);
        assert_eq!(renaming(&m), None);
        let list = m.overview().and_then(|o| o.devices.clone()).unwrap_or_default();
        assert_eq!(list.get(1).map(|d| d.name.as_str()), Some("BEAST"));
        m.update(Event::StartRename(other.id));
        type_name(&mut m, "Another");
        m.update(Event::SaveRename);
        m.update(Event::CancelRename);
        assert!(renaming(&m).is_some_and(|r| r.busy));
        // An answer for another computer is ignored; so is a rename of a computer that isn't listed.
        m.update(Event::Renamed(DeviceId::generate(), 1, Ok(computer("x", false))));
        assert!(renaming(&m).is_some());
        // The answer of a busy rename reaches the list even when its field is gone or it is another try's.
        let late = DeviceInfo { name: "Late".into(), ..other.clone() };
        m.update(Event::Renamed(other.id, 99, Ok(late.clone())));
        assert!(renaming(&m).is_some_and(|r| r.busy), "another try's answer leaves the field");
        let list = m.overview().and_then(|o| o.devices.clone()).unwrap_or_default();
        assert_eq!(list.get(1), Some(&late));
        m.update(Event::Renamed(other.id, 1, Ok(late)));
        assert_eq!(renaming(&m), None);
        let mut unknown = signed_in();
        assert!(unknown.update(Event::StartRename(DeviceId::generate())).is_empty());
        assert_eq!(unknown.overview().and_then(|o| o.renaming.clone()), None);
    }

    /// A late answer of an earlier rename must not close the field of a later one, even when the overview was replaced in
    /// between (leaving for another page and coming back starts a new overview).
    #[test]
    fn a_late_rename_answer_does_not_close_the_field_of_a_newer_rename() {
        let (mut m, this, other) = with_two_computers();
        m.update(Event::StartRename(other.id));
        type_name(&mut m, "Studio PC");
        let effects = m.update(Event::SaveRename);
        assert_eq!(effects, vec![Effect::RenameDevice { id: other.id, name: "Studio PC".into(), ticket: 1 }]);
        m.update(Event::Tab(Tab::Trash));
        m.update(Event::Tab(Tab::Overview));
        m.update(Event::Devices(Ok(vec![this, other.clone()])));
        m.update(Event::StartRename(other.id));
        type_name(&mut m, "Second name");
        let effects = m.update(Event::SaveRename);
        assert_eq!(effects, vec![Effect::RenameDevice { id: other.id, name: "Second name".into(), ticket: 2 }]);
        // The first answer comes late: the second rename is still waiting.
        m.update(Event::Renamed(other.id, 1, Ok(DeviceInfo { name: "Studio PC".into(), ..other.clone() })));
        assert!(renaming(&m).is_some_and(|r| r.busy && r.ticket == 2));
    }

    #[test]
    fn a_computer_that_is_gone_from_the_list_ends_its_rename() {
        let (mut m, this, other) = with_two_computers();
        m.update(Event::StartRename(other.id));
        m.update(Event::Devices(Ok(vec![this])));
        assert_eq!(renaming(&m), None);
    }

    #[test]
    fn sign_out_other_computers_opens_the_security_page_at_its_question() {
        let (mut m, _, _) = with_two_computers();
        m.update(Event::Tab(Tab::Computers));
        let effects = m.update(Event::SignOutOtherComputers);
        assert!(effects.contains(&Effect::Security(SecurityCall::Status)), "{effects:?}");
        let Screen::Security(page) = &m.screen else { panic!("{:?}", m.screen) };
        assert_eq!(page.step, crate::security::Step::SignOutOthers);
        // Only from the signed-in page.
        let mut out = model();
        assert!(out.update(Event::SignOutOtherComputers).is_empty());
    }

    #[test]
    fn only_a_version_that_is_surely_older_is_called_older() {
        assert!(is_older_version("0.0.9", "0.1.0"));
        assert!(is_older_version("0.1", "0.1.1"));
        assert!(is_older_version("0.1.0-pre", "0.2.0"));
        assert!(is_older_version("1.9.9", "1.10.0"), "numbers, not text");
        assert!(!is_older_version("0.1.0", "0.1.0"));
        assert!(!is_older_version("0.1.0.0", "0.1"));
        assert!(!is_older_version("0.2.0", "0.1.0"));
        assert!(!is_older_version("", "0.1.0"), "nothing was sent");
        assert!(!is_older_version("nightly", "0.1.0"));
        assert!(!is_older_version("0.0.x", "0.1.0"));
        assert!(!is_older_version("0.0.9", "unknown"));
    }

    #[test]
    fn leaving_the_computers_tab_drops_a_typed_name_but_not_one_on_its_way() {
        let (mut m, _, other) = with_two_computers();
        m.update(Event::Tab(Tab::Computers));
        m.update(Event::StartRename(other.id));
        m.update(Event::Tab(Tab::Overview));
        assert_eq!(renaming(&m), None);
        m.update(Event::Tab(Tab::Computers));
        m.update(Event::StartRename(other.id));
        type_name(&mut m, "Another");
        m.update(Event::SaveRename);
        m.update(Event::Tab(Tab::Overview));
        assert!(renaming(&m).is_some_and(|r| r.busy));
    }

    #[test]
    fn the_security_pages_list_is_asked_again_after_the_other_computers_were_signed_out() {
        use crate::security::SecurityAnswer;
        let (mut m, _, _) = with_two_computers();
        m.update(Event::OpenSecurity);
        let done = craft_cloud_app::RevokedOthers { revoked: 1 };
        let effects = m.update(Event::SecurityAnswer(SecurityAnswer::SignOutOthers(Ok(done))));
        assert!(effects.contains(&Effect::Devices), "{effects:?}");
        let effects = m.update(Event::SecurityAnswer(SecurityAnswer::ChangePassword(Ok(()))));
        assert!(effects.contains(&Effect::Devices), "{effects:?}");
        let effects = m.update(Event::SecurityAnswer(SecurityAnswer::Disable(Ok(()))));
        assert!(effects.contains(&Effect::Devices), "{effects:?}");
        let refused = Error::Refused { code: "wrong_password".into(), message: "no".into(), name_taken: None };
        let effects = m.update(Event::SecurityAnswer(SecurityAnswer::ChangePassword(Err(refused))));
        assert!(!effects.contains(&Effect::Devices), "{effects:?}");
    }

    /// Ctrl+1 to Ctrl+9 count the tabs that are visible, in the header's order.
    #[test]
    fn a_tab_number_goes_to_the_visible_tab_at_that_place() {
        let mut m = signed_in();
        let order = m.tabs();
        for (index, tab) in order.iter().enumerate() {
            m.update(Event::TabNumber(index + 1));
            assert_eq!(m.tab(), Some(*tab), "number {}", index + 1);
        }
        // A member has eight tabs: the places after them are nothing, and so is 0.
        let before = m.tab();
        for number in [0, order.len() + 1, 9, usize::MAX] {
            assert!(m.update(Event::TabNumber(number)).is_empty(), "{number}");
            assert_eq!(m.tab(), before);
        }
        // An admin's Admin tab is the ninth; the hidden one is skipped for a member, so the numbers don't shift.
        let mut admin = signed_in_admin();
        assert_eq!(admin.tabs().last(), Some(&Tab::Admin));
        admin.update(Event::TabNumber(9));
        assert_eq!(admin.tab(), Some(Tab::Admin));
        let mut member = signed_in();
        member.update(Event::TabNumber(9));
        assert_eq!(member.tab(), Some(Tab::Overview));
        // The same tab again is no new load.
        assert!(member.update(Event::TabNumber(1)).is_empty());
        // Not signed in: no tabs.
        assert!(model().update(Event::TabNumber(1)).is_empty());
    }

    /// A page that can't be left by a tab can't be left by a number either.
    #[test]
    fn a_tab_number_waits_while_the_tabs_are_locked() {
        let mut m = signed_in();
        m.update(Event::Tab(Tab::Security));
        let Screen::Security(page) = &mut m.screen else { panic!() };
        page.step = SecurityStep::Codes { codes: vec!["AAAA-BBBB".into()], why: crate::security::CodesWhy::TurnedOn };
        assert!(m.update(Event::TabNumber(1)).is_empty());
        assert_eq!(m.tab(), Some(Tab::Security));
        m.update(Event::Security(SecurityEvent::Home));
        m.update(Event::TabNumber(1));
        assert_eq!(m.tab(), Some(Tab::Overview));
    }

    /// Ctrl+R does what the page's Refresh button does, and nothing where there is none.
    #[test]
    fn refresh_page_is_the_refresh_button_of_the_page_shown() {
        let mut m = signed_in();
        assert_eq!(m.refresh_event(), Some(Event::RefreshUsage));
        m.update(Event::Tab(Tab::Computers));
        assert_eq!(m.refresh_event(), Some(Event::RefreshDevices));
        assert!(m.update(Event::RefreshPage).contains(&Effect::Devices));
        for (tab, want) in [
            (Tab::Files, Some(Event::LoadFiles)),
            (Tab::Trash, Some(Event::LoadTrash)),
            (Tab::Activity, Some(Event::Audit(AuditMsg::ActivityReload))),
            (Tab::Versions, None),
            (Tab::Security, None),
        ] {
            m.update(Event::Tab(tab));
            assert_eq!(m.refresh_event(), want, "{tab:?}");
        }
        // On the Files page it asks the agent again (once it is not loading).
        m.update(Event::Tab(Tab::Files));
        let Screen::Files(files) = &mut m.screen else { panic!() };
        files.loading = false;
        let effects = m.update(Event::RefreshPage);
        assert!(matches!(effects.as_slice(), [Effect::Recents { .. }, Effect::Conflicts { .. }]), "{effects:?}");
        assert!(model().update(Event::RefreshPage).is_empty(), "not signed in");
    }

    #[test]
    fn refresh_page_on_the_admin_pages_follows_the_part_shown() {
        let mut m = signed_in_admin();
        m.update(Event::Tab(Tab::Admin));
        assert_eq!(m.refresh_event(), Some(Event::Admin(AdminEvent::Reload)));
        let Screen::Admin(admin) = &mut m.screen else { panic!() };
        admin.view = crate::admin::View::Create(Default::default());
        assert_eq!(m.refresh_event(), None);
    }

    /// Ctrl+F has a field to go to on the pages that show one.
    #[test]
    fn only_pages_with_a_search_field_have_one() {
        let mut m = files_loaded();
        assert!(m.has_search());
        m.update(Event::Tab(Tab::Trash));
        assert!(!m.has_search(), "the trash list is not loaded yet");
        let Screen::Trash(trash) = &mut m.screen else { panic!() };
        trash.page = Some(TrashResponse { items: Vec::new(), next: None });
        assert!(m.has_search());
        for tab in [Tab::Overview, Tab::Computers, Tab::Security, Tab::Versions] {
            m.update(Event::Tab(tab));
            assert!(!m.has_search(), "{tab:?}");
        }
        let mut admin = signed_in_admin();
        admin.update(Event::Tab(Tab::Admin));
        assert!(admin.has_search(), "the accounts' search");
    }

    fn event_of(action: &str, device: Option<DeviceId>) -> AuditEvent {
        AuditEvent { actor_device: device, ..crate::agent::fake::FakeAgent::sample_event(1, action) }
    }

    #[test]
    fn the_activity_filters_belong_to_their_page_and_ctrl_f_finds_them_once_events_are_shown() {
        use crate::filter::{ActivityKind, FilterEdit};
        let mut m = signed_in();
        assert!(!m.has_search(), "nothing is loaded");
        let session = m.overview().unwrap().session;
        m.update(Event::Recent { session, result: Ok(AuditPage { events: vec![event_of("auth.login", None)], next: None }) });
        assert!(m.has_search());
        m.update(Event::RecentFilter(FilterEdit::Kind(ActivityKind::Admin)));
        m.update(Event::RecentFilter(FilterEdit::Search("x".into())));
        let filter = m.overview().unwrap().recent_filter.clone();
        assert_eq!((filter.kind, filter.search.as_str()), (ActivityKind::Admin, "x"));
        // The other table is not touched, and the event does nothing on a page without the table.
        m.update(Event::Tab(Tab::Security));
        let Screen::Security(page) = &mut m.screen else { panic!() };
        assert!(page.activity_filter.is_off());
        m.update(Event::SecurityActivityFilter(FilterEdit::Kind(ActivityKind::Account)));
        m.update(Event::RecentFilter(FilterEdit::Clear));
        let Screen::Security(page) = &m.screen else { panic!() };
        assert_eq!(page.activity_filter.kind, ActivityKind::Account);
        assert_eq!(page.overview.recent_filter.kind, ActivityKind::Admin, "the overview's filters were not cleared from the other page");
        assert!(!m.has_search(), "its events are not loaded");
        m.update(Event::SecurityAnswer(SecurityAnswer::Activity(Ok(AuditPage { events: vec![event_of("auth.login", None)], next: None }))));
        assert!(m.has_search());
    }

    #[test]
    fn the_zoom_steps_by_a_tenth_between_80_and_160_percent_and_resets() {
        let mut m = model();
        assert_eq!(m.zoom_factor(), 1.0);
        for _ in 0..20 {
            m.update(Event::Zoom(ZoomStep::In));
        }
        assert_eq!(m.zoom_factor(), 1.6, "no more than 160 %");
        m.update(Event::Zoom(ZoomStep::Out));
        assert_eq!(m.zoom_factor(), 1.5);
        m.update(Event::Zoom(ZoomStep::Reset));
        assert_eq!(m.zoom_factor(), 1.0);
        for _ in 0..20 {
            assert!(m.update(Event::Zoom(ZoomStep::Out)).is_empty());
        }
        assert_eq!(m.zoom_factor(), 0.8, "no less than 80 %");
        m.update(Event::Zoom(ZoomStep::In));
        assert_eq!(m.zoom_factor(), 0.9);
        // On every screen, signed in or not, and a step changes nothing else.
        let mut signed = signed_in();
        let before = signed.screen.clone();
        signed.update(Event::Zoom(ZoomStep::In));
        assert_eq!((signed.zoom_factor(), signed.screen == before), (1.1, true));
    }
}
