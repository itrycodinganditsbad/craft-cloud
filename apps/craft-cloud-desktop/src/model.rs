//! The app's state, as a pure function of what happened. [`Model::update`] takes an [`Event`] (the agent's answer to a
//! call, a click, a tick of the 2 s clock, a second start, the window's close button) and returns the [`Effect`]s to
//! carry out: calls of the agent, opening the folder, quitting the app, the app's own start-at-login entry. Nothing
//! here does I/O or reads a clock, so every screen and every way between them is a unit test.
//!
//! The window is a view of [`Model::screen`]; text fields edit its forms in place, buttons send events.

use std::path::Path;

use craft_cloud_app::{DeviceId, DeviceInfo, Error, JoinRequest, PairingInfo, PairingState, PairingStatus, RunState, Status, Uuid, codes};
use craft_cloud_proto::api::codes as api_codes;
use craft_cloud_proto::join::JoinString;

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
    /// Signed in: the account, the sync, the computers.
    SignedIn(Overview),
    /// Showing an add-a-computer code.
    AddingComputer(Adding),
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

/// The join form, as typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoinForm {
    /// The join string (or the link) as typed or pasted.
    pub join: String,
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

impl JoinForm {
    fn new(defaults: &JoinDefaults) -> Self {
        Self { folder: defaults.folder.clone(), device_name: defaults.device_name.clone(), ..Self::default() }
    }

    /// The request for the agent: the join string as read (a link, spaces and lower case put right), the folder, the
    /// name (the agent's default when empty), and `adopt` if the user agreed to it for this folder.
    fn request(&self, password: Option<String>) -> JoinRequest {
        let folder = self.folder.trim().to_owned();
        let name = self.device_name.trim();
        JoinRequest {
            join: self.join.clone(),
            adopt: self.adopt_for.as_deref() == Some(folder.as_str()),
            folder: Some(folder),
            device_name: (!name.is_empty()).then(|| name.to_owned()),
            password,
        }
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

/// The signed-in view.
#[derive(Debug, Clone, PartialEq)]
pub struct Overview {
    pub status: Status,
    /// The account's computers, once asked.
    pub devices: Option<Vec<DeviceInfo>>,
    pub devices_error: Option<String>,
    /// The computer being removed.
    pub removing: Option<DeviceId>,
    /// What went wrong with the last action (sign out, pause), for people.
    pub error: Option<String>,
    /// Signing out failed because the server can't be reached: offer to forget the account here only.
    pub offer_local_sign_out: bool,
}

impl Overview {
    fn new(status: Status) -> Self {
        Self { status, devices: None, devices_error: None, removing: None, error: None, offer_local_sign_out: false }
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
    AutostartSet(Result<Status, Error>),
    PairingMade(Result<PairingInfo, Error>),
    PairingPolled(Result<PairingStatus, Error>),
    Devices(Result<Vec<DeviceInfo>, Error>),
    SignedOut(Result<Status, Error>),
    /// [`Effect::Pause`]'s or [`Effect::Resume`]'s answer.
    Answered(Result<Status, Error>),
    /// [`Effect::QuitAgent`] is done (whatever came of it).
    AgentQuit,
    /// [`Effect::AppAutostart`]'s outcome.
    AppAutostartSet(Result<(), String>),

    /// Join (the form's button, or Enter).
    SubmitJoin,
    /// The answer to "this folder has files in it: sync them too?".
    SyncExistingFiles(bool),
    SubmitPassword,
    CancelPassword,
    OpenFolder,
    AddComputer,
    /// Done or Cancel on the add-a-computer view.
    CloseAdding,
    RemoveDevice(DeviceId),
    /// Ask for the computer list again.
    RefreshDevices,
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
}

/// What to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    /// Find the running agent ([`Event::Found`]).
    Connect,
    /// Start the agent hidden from this app's folder and wait up to 5 s for it ([`Event::AgentStarted`]).
    StartAgent,
    Status,
    Join(JoinRequest),
    SetAutostart(bool),
    Pairing,
    PairingState(Uuid),
    CancelPairing(Uuid),
    Devices,
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
    defaults: JoinDefaults,
    polling: bool,
    polling_pairing: bool,
    failed_polls: u32,
    /// The app's start-at-login entry as last asked for.
    app_autostart: Option<bool>,
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
            defaults,
            polling: false,
            polling_pairing: false,
            failed_polls: 0,
            app_autostart: None,
        }
    }

    /// The signed-in view, wherever it is shown.
    pub fn overview(&self) -> Option<&Overview> {
        match &self.screen {
            Screen::SignedIn(overview) => Some(overview),
            Screen::AddingComputer(adding) => Some(&adding.overview),
            _ => None,
        }
    }

    fn overview_mut(&mut self) -> Option<&mut Overview> {
        match &mut self.screen {
            Screen::SignedIn(overview) => Some(overview),
            Screen::AddingComputer(adding) => Some(&mut adding.overview),
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
            Event::Joined(result) => self.joined(result),
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

            Event::SubmitJoin => self.submit_join(),
            Event::SyncExistingFiles(yes) => {
                let Screen::Join(form) = &mut self.screen else { return Vec::new() };
                form.ask_adopt = false;
                if !yes {
                    return Vec::new();
                }
                form.adopt_for = Some(form.folder.trim().to_owned());
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
            Event::RefreshDevices => self.overview().map(|_| Effect::Devices).into_iter().collect(),
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
                if self.busy == Some(Busy::Quitting) {
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
                Screen::SignedIn(overview) => overview.status = status,
                Screen::AddingComputer(adding) => adding.overview.status = status,
                // The join's own answer moves on from there.
                Screen::Joining { .. } => {}
                _ => {
                    self.screen = Screen::SignedIn(Overview::new(status));
                    effects.push(Effect::Devices);
                }
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
            (_, Screen::Join(_) | Screen::Joining { .. } | Screen::PasswordNeeded(_)) => {}
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
            Error::Refused { .. } => false,
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
        let e = match result {
            Ok(status) => {
                self.screen = Screen::SignedIn(Overview::new(status.clone()));
                let mut effects = self.status_came(status);
                // Start at login: the agent's entry (the app's follows it).
                effects.extend([Effect::SetAutostart(true), Effect::Devices]);
                return effects;
            }
            Err(e) => e,
        };
        let (code, text) = match &e {
            Error::Refused { code, message } => (code.as_str(), message.clone()),
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
                form.error = Some(text);
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
        if self.busy.is_some() {
            return Vec::new();
        }
        let Some(overview) = self.overview_mut() else { return Vec::new() };
        overview.error = None;
        overview.offer_local_sign_out = false;
        self.busy = Some(Busy::SigningOut);
        vec![Effect::Logout { local }]
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
                match &mut self.screen {
                    Screen::SignedIn(overview) => {
                        overview.error = Some(text);
                        overview.offer_local_sign_out = offline;
                    }
                    Screen::AddingComputer(adding) => adding.overview.error = Some(text),
                    Screen::Error(problem) => problem.details = format!("{}\n\nSigning out failed: {text}", problem.details),
                    _ => {}
                }
                Vec::new()
            }
        }
    }
}

/// An error's message, for people.
fn message(e: &Error) -> String {
    match e {
        Error::Refused { message, .. } => message.clone(),
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

    fn refused(code: &str, message: &str) -> Error {
        Error::Refused { code: code.into(), message: message.into() }
    }

    /// A model past the start, on the join form.
    fn on_join_form() -> Model {
        let mut m = model();
        assert_eq!(m.update(Event::Start), vec![Effect::Connect]);
        m.update(Event::Found(Ok(status(false))));
        assert!(matches!(m.screen, Screen::Join(_)), "{:?}", m.screen);
        m
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
            DeviceInfo { id: DeviceId::generate(), name: "lptp1".into(), created_ms: 1, last_used_ms: 2, revoked: false, current: true },
            DeviceInfo { id: DeviceId::generate(), name: "BEAST".into(), created_ms: 1, last_used_ms: 3, revoked: false, current: false },
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
        let o = m.overview().unwrap();
        assert!(o.offer_local_sign_out && o.error.is_some());
        assert_eq!(m.update(Event::SignOutHere), vec![Effect::Logout { local: true }]);
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
        assert_eq!(m.update(Event::Found(Ok(on.clone()))), vec![Effect::AppAutostart(true), Effect::Devices]);
        assert!(m.update(Event::Polled(Ok(on.clone()))).is_empty());
        assert_eq!(m.update(Event::Polled(Ok(Status { autostart: Some(false), ..on }))), vec![Effect::AppAutostart(false)]);
    }

    #[test]
    fn first_sentences_make_one_line() {
        assert_eq!(first_sentence("can't reach it. Try later"), "Can't reach it.");
        assert_eq!(first_sentence("no dot here"), "No dot here");
        assert_eq!(sentence(""), "");
    }
}
