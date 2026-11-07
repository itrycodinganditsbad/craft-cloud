//! The app's state, as a pure function of what happened. [`Model::update`] takes an [`Event`] (the agent's answer to a
//! call, a click, a tick of the 2 s clock, a second start, the window's close button) and returns the [`Effect`]s to
//! carry out: calls of the agent, opening the folder, quitting the app, the app's own start-at-login entry. Nothing
//! here does I/O or reads a clock, so every screen and every way between them is a unit test.
//!
//! The window is a view of [`Model::screen`]; text fields edit its forms in place, buttons send events.

use std::path::Path;

use craft_cloud_app::{
    AccountUsage, AuditEvent, AuditPage, DeviceId, DeviceInfo, Error, HistoryResponse, JoinRequest, LoginRequest, NodeId, PairingInfo,
    PairingState, PairingStatus, RestoreResponse, RunState, SecondFactorLoginRequest, Status, TrashResponse, TrashRestoreResponse, UserQuery,
    Uuid, VersionId, codes,
};
use craft_cloud_proto::api::AdminConfirm;
use craft_cloud_proto::api::codes as api_codes;
use craft_cloud_proto::join::JoinString;

use crate::admin::{Access, Action, AdminEvent, AdminScreen};
use crate::audit::{ActivityScreen, AuditMsg};
use crate::names::Names;
use crate::security::{SecondFactorForm, SecurityAnswer, SecurityCall, SecurityEvent, SecurityPage, Step as SecurityStep};

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
pub const CHALLENGE_EXPIRED: &str = "The sign-in took too long. Sign in again.";
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

/// The join form, as typed.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct JoinForm {
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
            .field("mode", &self.mode)
            .field("join", &"<redacted>")
            .field("server", &self.server)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("challenge", &self.challenge.as_ref().map(|_| "<redacted>"))
            .field("folder", &self.folder)
            .field("device_name", &self.device_name)
            .field("error", &self.error)
            .field("ask_adopt", &self.ask_adopt)
            .field("adopt_for", &self.adopt_for)
            .finish()
    }
}

/// One step of the first start's rail: what is known of it so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RailStep {
    pub title: &'static str,
    /// What is known (or what the step is for, while it is not).
    pub detail: String,
    pub done: bool,
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

    /// The server's address for the agent: as typed, with `https://` when it has no scheme (as in a join string).
    fn server_address(&self) -> String {
        let typed = self.server.trim();
        if typed.contains("://") { typed.to_owned() } else { format!("https://{typed}") }
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

    /// The three steps of the first start with what is known of each. No probe of the server is made, so the server is
    /// shown as typed (or as the join string names it), never as "reachable".
    pub fn rail(&self) -> [RailStep; 3] {
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
        // Whether it is a full path is said when the form is sent, so the rail doesn't depend on the platform's paths.
        let folder_known = !folder.is_empty();
        [
            RailStep { title: "Server", done: server.is_some(), detail: server.unwrap_or_else(|| "Where your Craft Cloud lives".into()) },
            RailStep { title: "Account", done: account.is_some(), detail: account.unwrap_or_else(|| "Sign in, or use a join string".into()) },
            RailStep {
                title: "Folder and name",
                done: folder_known,
                detail: if folder_known { folder.to_owned() } else { "Where your files live on this computer".into() },
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

/// The pages of the signed-in window, as the header's tabs list them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    /// File version history ([`Screen::Versions`]).
    Versions,
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
    /// What went wrong with the last action (pause), for people.
    pub error: Option<String>,
    /// Whether the server lets this account use the admin pages (asked at sign-in).
    pub access: Access,
    /// Which sign-in this view is of: answers to the admin check made for another one are ignored.
    pub session: u64,
    /// The newest events of the account's activity, for the home page.
    pub recent: Recent,
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
            error: None,
            access: Access::Unknown,
            session: crate::admin::fresh(),
            recent: Recent::Loading,
        }
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
    /// The form's switch between Sign in and Join string.
    SetJoinMode(JoinMode),
    /// The answer to "this folder has files in it: sync them too?".
    SyncExistingFiles(bool),
    SubmitPassword,
    CancelPassword,
    OpenFolder,
    OpenVersions,
    OpenTrash,
    VersionsPath(String),
    LoadVersions,
    NextVersions,
    BackVersions,
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
    /// Ask for the computer list again.
    RefreshDevices,
    RefreshUsage,
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
        vec![Effect::Devices, Effect::Account { session }, self.next_usage_request(), Effect::Recent { session }]
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
            Screen::Security(_) => Some(Tab::Security),
            Screen::Activity(_) => Some(Tab::Activity),
            Screen::Admin(_) => Some(Tab::Admin),
            _ => None,
        }
    }

    /// The header's tabs: all the pages, the admin pages for an admin only.
    pub fn tabs(&self) -> Vec<Tab> {
        let all = [Tab::Overview, Tab::Versions, Tab::Trash, Tab::Computers, Tab::Security, Tab::Activity, Tab::Admin];
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
            Screen::Security(_) => self.security(SecurityEvent::Close),
            Screen::Activity(_) => self.audit(AuditMsg::CloseActivity),
            Screen::Admin(_) => self.admin(AdminEvent::Close),
            Screen::AddingComputer(_) => self.update(Event::CloseAdding),
            _ => Vec::new(),
        };
        let Screen::SignedIn(overview) = &mut self.screen else { return effects };
        overview.page = if tab == Tab::Computers { OverviewPage::Computers } else { OverviewPage::Home };
        effects.extend(match tab {
            Tab::Overview | Tab::Computers => Vec::new(),
            Tab::Versions => self.update(Event::OpenVersions),
            Tab::Trash => self.update(Event::OpenTrash),
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
            _ => None,
        }
    }

    pub fn update(&mut self, event: Event) -> Vec<Effect> {
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
                page.answer(answer, now_ms).into_iter().map(Effect::Security).collect()
            }
            Event::SubmitSecondFactor => self.submit_second_factor(),
            Event::CancelSecondFactor => {
                if let Screen::SecondFactor(form) = &self.screen {
                    self.screen = match &form.password {
                        Some(pw) => Screen::PasswordNeeded(pw.clone()),
                        None => Screen::Join(JoinForm { challenge: None, password: String::new(), ..form.form.clone() }),
                    };
                }
                Vec::new()
            }
            Event::SubmitJoin => self.submit_join(),
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
                self.screen = Screen::SignedIn(adding.overview.clone());
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
            Event::RefreshDevices => {
                let Some(overview) = self.overview() else { return Vec::new() };
                // An admin check that couldn't be made at sign-in is made again.
                let again = (overview.access == Access::Unknown).then_some(Effect::Account { session: overview.session });
                std::iter::once(Effect::Devices).chain(again).collect()
            }
            Event::Tab(tab) => self.open_tab(tab),
            Event::Admin(event) => self.admin(event),
            Event::Audit(msg) => self.audit(msg),
            Event::RefreshUsage => {
                let Some(overview) = self.overview_mut() else { return Vec::new() };
                if overview.usage_loading {
                    return Vec::new();
                }
                overview.usage_loading = true;
                overview.usage_error = None;
                vec![self.next_usage_request()]
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
                // The join's own answer moves on from there.
                Screen::Joining { .. } => {}
                Screen::Versions(_) | Screen::Trash(_) => {}
                _ => {
                    let overview = Overview::new(status);
                    let session = overview.session;
                    self.screen = Screen::SignedIn(overview);
                    effects.push(Effect::Devices);
                    // Whether the account is an admin, for the admin pages.
                    effects.push(Effect::Account { session });
                    effects.push(self.next_usage_request());
                    effects.push(Effect::Recent { session });
                }
            }
            if let Some(effect) = new_usage_request {
                effects.push(effect);
            }
            return effects;
        }
        match (&status.run_state, &self.screen) {
            (RunState::Paused { reason }, _) => {
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
                return Vec::new();
            }
        }
        if !Path::new(form.folder.trim()).is_absolute() {
            form.error = Some(FOLDER_NOT_ABSOLUTE.to_owned());
            return Vec::new();
        }
        let request = form.request(None);
        self.screen = Screen::Joining { form: form.clone(), password: None };
        vec![Effect::Join(request)]
    }

    /// The password sign-in: the fields are checked, then the password leaves the form with the request.
    fn submit_sign_in(&mut self) -> Vec<Effect> {
        let Screen::Join(form) = &mut self.screen else { return Vec::new() };
        let problem = if form.server.trim().is_empty() {
            Some(SERVER_EMPTY)
        } else if form.user.trim().is_empty() {
            Some(USER_EMPTY)
        } else if form.password.is_empty() {
            Some(SIGN_IN_PASSWORD_EMPTY)
        } else if !Path::new(form.folder.trim()).is_absolute() {
            Some(FOLDER_NOT_ABSOLUTE)
        } else {
            None
        };
        if let Some(problem) = problem {
            form.error = Some(problem.to_owned());
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
            let overview = page.overview.clone();
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
            (_, None) => {
                form.error = Some(if signing_in { crate::security::explain(&e) } else { text });
                Screen::Join(form)
            }
        };
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
        let locked =
            Error::RefusedWith { code: codes::TWO_FACTOR_LOCKED.into(), message: "locked".into(), two_factor: None, retry_after_secs: Some(840) };
        m.update(Event::LoggedIn(Err(locked)));
        let Screen::SecondFactor(second) = &m.screen else { panic!("{:?}", m.screen) };
        assert_eq!(second.error.as_deref(), Some("Too many wrong codes. Try again in 14 minutes."));
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
            (refused("offline", "connection refused"), "Can't reach the server. Try again when this computer is online."),
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
        assert_eq!((steps[0].title, steps[0].detail.as_str(), steps[0].done), ("Server", "cloud.shadows.ch", true));
        assert_eq!((steps[1].detail.as_str(), steps[1].done), ("fabio", true));
        assert!(steps[2].done && steps[2].detail == defaults().folder);
        let f = form(&mut m);
        f.server.clear();
        f.user.clear();
        f.folder.clear();
        let steps = f.rail();
        assert!(steps.iter().all(|s| !s.done));
        m.update(Event::SetJoinMode(JoinMode::JoinString));
        form(&mut m).join = "https://cloud.shadows.ch/join/k7m4-q9tx".into();
        let steps = form(&mut m).rail();
        assert_eq!((steps[0].detail.as_str(), steps[1].detail.as_str()), ("cloud.shadows.ch", "Join string"));
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
        let removed =
            Status { removed: true, run_state: RunState::Paused { reason: "the server no longer accepts this device".into() }, ..status(true) };
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
        m.update(Event::Found(Ok(Status { run_state: RunState::Paused { reason: reason.into() }, ..status(false) })));
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
        for tab in [Tab::Versions, Tab::Trash, Tab::Security, Tab::Activity, Tab::Computers] {
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
                Effect::Recent { session: m.overview().unwrap().session }
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
        let member = [Tab::Overview, Tab::Versions, Tab::Trash, Tab::Computers, Tab::Security, Tab::Activity];
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
        assert!(matches!(effects.as_slice(), [Effect::Devices, Effect::Account { .. }, Effect::Usage(_), Effect::Recent { .. }]), "{effects:?}");
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
}
