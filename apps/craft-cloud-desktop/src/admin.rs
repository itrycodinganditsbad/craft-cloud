//! The admin pages' state, as a pure function of what happened, like [`crate::model`] (which hosts it: the pages are
//! [`Screen::Admin`], and [`Event::Admin`] and the admin [`Effect`]s go through the same [`Model::update`]).
//!
//! - **Who sees them**: the pages are offered only when the server answers the admin counters
//!   ([`Effect::AdminServer`], asked at every sign-in; [`Access::Admin`]); a `forbidden` makes it [`Access::Member`].
//!   That is a convenience, not a check: the server decides every call again, and a refusal is shown as the server's.
//! - **Pages**: the users (a paged list with search), one user (role and quota, disable, enable, remove, invite and
//!   reset code, the user's devices), and a new account (name and role; its quota is set afterwards, on the user's
//!   page, as a change of its own).
//! - **Every change asks for the admin's password afresh** in a confirmation ([`Dialog`], holding a [`Confirmation`]):
//!   its text goes into the call as the body's `password` and is cleared from the model at once. Whatever else the
//!   server comes to want of a confirmation (a second factor's code) is another field of [`Confirmation`] and of
//!   [`Confirmation::take`]: the dialog, the events and the calls stay as they are.
//! - **A code is shown once** ([`ShownCode`]): an invite or a reset code is kept in the model only until the admin
//!   dismisses it (or leaves the pages), never written anywhere, and left out of `Debug` output.

use craft_cloud_app::{AdminCode, AdminCreatedUser, AdminServerInfo, AdminUserInfo, AdminUserList, DeviceId, DeviceInfo, Error, UserQuery, Uuid};
use craft_cloud_proto::api::{AdminConfirm, codes};

use crate::model::{Effect, Event, Model, Overview, Screen};

/// Accounts on one page of the list.
pub const PAGE_SIZE: u32 = 20;
/// The most a quota may be, in GiB (a petabyte: a typo, not a quota, past it).
pub const MAX_QUOTA_GIB: f64 = 1_048_576.0;
const GIB: f64 = 1_073_741_824.0;

/// Whether the signed-in account is an admin, as the server answered the admin counters.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Access {
    /// Not asked yet, or the server couldn't be reached: no admin pages.
    #[default]
    Unknown,
    /// The server refused the admin calls (`forbidden`).
    Member,
    /// The server answered: the admin pages are offered, with these counters.
    Admin(AdminServerInfo),
}

/// An account's role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoleChoice {
    #[default]
    Member,
    Admin,
}

impl RoleChoice {
    /// The server's name for it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Member => "member",
            Self::Admin => "admin",
        }
    }

    /// The role the server names (anything but `admin` reads as a member).
    pub fn of(role: &str) -> Self {
        if role == "admin" { Self::Admin } else { Self::Member }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Member => "Member",
            Self::Admin => "Admin",
        }
    }
}

/// The admin's confirmation of a change: today their current password. Its `Debug` output hides it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Confirmation {
    pub password: String,
}

impl std::fmt::Debug for Confirmation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Confirmation").field("password", &"<redacted>").finish()
    }
}

impl Confirmation {
    /// Whether enough was typed to send.
    pub fn ready(&self) -> bool {
        !self.password.is_empty()
    }

    /// The confirmation as the calls send it, with what was typed cleared from here.
    pub fn take(&mut self) -> AdminConfirm {
        AdminConfirm { password: Some(std::mem::take(&mut self.password)), ..AdminConfirm::default() }
    }
}

/// A change the admin can make, with what its confirmation page says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    CreateUser {
        username: String,
        role: RoleChoice,
    },
    /// A new role and/or quota (`quota`: `Some(None)` removes it).
    UpdateUser {
        id: Uuid,
        username: String,
        role: Option<RoleChoice>,
        quota: Option<Option<u64>>,
    },
    Disable {
        id: Uuid,
        username: String,
    },
    Enable {
        id: Uuid,
        username: String,
    },
    Remove {
        id: Uuid,
        username: String,
    },
    IssueInvite {
        id: Uuid,
        username: String,
    },
    RevokeInvite {
        id: Uuid,
        username: String,
    },
    IssueReset {
        id: Uuid,
        username: String,
    },
    RevokeDevice {
        id: Uuid,
        username: String,
        device: DeviceId,
        device_name: String,
    },
}

impl Action {
    /// The confirmation page's title.
    pub fn title(&self) -> String {
        match self {
            Self::CreateUser { username, .. } => format!("Create {username}"),
            Self::UpdateUser { username, .. } => format!("Change {username}"),
            Self::Disable { username, .. } => format!("Disable {username}"),
            Self::Enable { username, .. } => format!("Enable {username}"),
            Self::Remove { username, .. } => format!("Remove {username}"),
            Self::IssueInvite { username, .. } => format!("New invite for {username}"),
            Self::RevokeInvite { username, .. } => format!("Revoke the invite of {username}"),
            Self::IssueReset { username, .. } => format!("Password reset code for {username}"),
            Self::RevokeDevice { username, device_name, .. } => format!("Sign {device_name} of {username} out"),
        }
    }

    /// What the change does, in a sentence or two.
    pub fn explain(&self) -> String {
        match self {
            Self::CreateUser { role, .. } => {
                format!("A new {} account without a password. You get an invite code to give them, shown once.", role.as_str())
            }
            Self::UpdateUser { role, quota, .. } => {
                let mut parts = Vec::new();
                if let Some(role) = role {
                    parts.push(format!("the role becomes {}", role.as_str()));
                }
                match quota {
                    Some(Some(bytes)) => parts.push(format!("the quota becomes {}", format_bytes(*bytes))),
                    Some(None) => parts.push("the quota is removed".to_owned()),
                    None => {}
                }
                sentence(&parts.join(" and "))
            }
            Self::Disable { .. } => "Their computers are signed out and they can't sign in until you enable the account again.".into(),
            Self::Enable { .. } => "They can sign in again. Computers that were signed out stay signed out.".into(),
            Self::Remove { .. } => "The account is marked for removal and its computers are signed out. Nothing it stored is deleted.".into(),
            Self::IssueInvite { .. } => "A new invite code for an account that has no password yet, shown once. It replaces the open one.".into(),
            Self::RevokeInvite { .. } => "The account's open invite or password reset code stops working.".into(),
            Self::IssueReset { .. } => {
                "A code good for an hour that sets a new password. Using it signs every computer of the account out. Shown once.".into()
            }
            Self::RevokeDevice { .. } => "That computer can't sync any more; its files stay there.".into(),
        }
    }

    /// The label of the confirm button.
    pub fn button(&self) -> &'static str {
        match self {
            Self::CreateUser { .. } => "Create account",
            Self::UpdateUser { .. } => "Save changes",
            Self::Disable { .. } => "Disable",
            Self::Enable { .. } => "Enable",
            Self::Remove { .. } => "Remove account",
            Self::IssueInvite { .. } => "Make an invite",
            Self::RevokeInvite { .. } => "Revoke",
            Self::IssueReset { .. } => "Make a reset code",
            Self::RevokeDevice { .. } => "Sign out",
        }
    }
}

/// What a change came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Created(AdminCreatedUser),
    Updated(AdminUserInfo),
    Code(AdminCode),
    Done,
}

/// The password page of one change.
#[derive(Debug, Clone, PartialEq)]
pub struct Dialog {
    pub action: Action,
    pub confirmation: Confirmation,
    /// What went wrong with the last try, for people.
    pub error: Option<String>,
    /// The call is with the server: nothing can be sent twice.
    pub busy: bool,
}

/// What kind of code is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeKind {
    Invite,
    PasswordReset,
}

/// A code the server made, shown once. Its `Debug` output hides the code and the link.
#[derive(Clone, PartialEq, Eq)]
pub struct ShownCode {
    pub username: String,
    pub kind: CodeKind,
    pub code: String,
    /// The whole join string, when the server knows its public address.
    pub join_url: Option<String>,
    pub expires_ms: i64,
}

impl std::fmt::Debug for ShownCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShownCode")
            .field("username", &self.username)
            .field("kind", &self.kind)
            .field("code", &"<redacted>")
            .field("join_url", &self.join_url.as_ref().map(|_| "<redacted>"))
            .field("expires_ms", &self.expires_ms)
            .finish()
    }
}

/// The list of accounts: one page, the search that made it, and the cursors back to the first.
#[derive(Debug, Clone, PartialEq)]
pub struct UserList {
    /// The search field, as typed.
    pub search: String,
    /// The search the shown page was asked with.
    pub applied: String,
    pub users: Vec<AdminUserInfo>,
    /// How each page up to this one was asked for: `cursors[0]` is `None`, the last is this page's.
    pub cursors: Vec<Option<String>>,
    /// The cursor of the page after this one, if there is one.
    pub next: Option<String>,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
    /// The number of the call whose answer is wanted; older answers are ignored.
    ticket: u64,
}

impl UserList {
    fn new() -> Self {
        Self {
            search: String::new(),
            applied: String::new(),
            users: Vec::new(),
            cursors: vec![None],
            next: None,
            loading: false,
            loaded: false,
            error: None,
            ticket: 0,
        }
    }

    /// The page number shown (from 1).
    pub fn page(&self) -> usize {
        self.cursors.len()
    }

    pub fn has_previous(&self) -> bool {
        self.cursors.len() > 1
    }

    /// Asks for the current page.
    fn load(&mut self) -> Effect {
        self.loading = true;
        self.error = None;
        self.ticket = self.ticket.wrapping_add(1);
        let query = UserQuery {
            cursor: self.cursors.last().cloned().flatten(),
            limit: Some(PAGE_SIZE),
            query: (!self.applied.is_empty()).then(|| self.applied.clone()),
        };
        Effect::AdminUsers { query, ticket: self.ticket }
    }

    fn find(&self, id: Uuid) -> Option<&AdminUserInfo> {
        self.users.iter().find(|u| u.id == id)
    }
}

/// The form for a new account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CreateForm {
    pub username: String,
    pub role: RoleChoice,
    pub error: Option<String>,
}

/// One account's page: its role and quota to change, and its devices.
#[derive(Debug, Clone, PartialEq)]
pub struct UserView {
    pub id: Uuid,
    pub username: String,
    /// The role field.
    pub role: RoleChoice,
    /// The quota field, in GiB (empty: none).
    pub quota: String,
    /// What the quota field showed when the page was opened or last saved, to tell whether it was changed.
    pub quota_shown: String,
    pub error: Option<String>,
    pub devices: Option<Vec<DeviceInfo>>,
    pub devices_error: Option<String>,
}

impl UserView {
    fn new(user: &AdminUserInfo) -> Self {
        let quota = user.quota_bytes.map(quota_text).unwrap_or_default();
        Self {
            id: user.id,
            username: user.username.clone(),
            role: RoleChoice::of(&user.role),
            quota_shown: quota.clone(),
            quota,
            error: None,
            devices: None,
            devices_error: None,
        }
    }
}

/// Which page of the admin pages is shown.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    List,
    Create(CreateForm),
    User(Box<UserView>),
}

/// The admin pages.
#[derive(Debug, Clone, PartialEq)]
pub struct AdminScreen {
    /// The signed-in view the pages came from and go back to.
    pub overview: Overview,
    pub list: UserList,
    pub view: View,
    /// A change waiting for the admin's password.
    pub dialog: Option<Dialog>,
    /// A code just made, until it is dismissed.
    pub code: Option<ShownCode>,
}

/// What happened on the admin pages: clicks and the answers of the admin calls.
#[derive(Debug, Clone, PartialEq)]
pub enum AdminEvent {
    /// [`Effect::AdminServer`]'s answer.
    Server(Result<AdminServerInfo, Error>),
    /// [`Effect::AdminUsers`]'s answer.
    Users {
        ticket: u64,
        result: Result<AdminUserList, Error>,
    },
    /// [`Effect::AdminDevices`]'s answer.
    Devices {
        user: Uuid,
        result: Result<Vec<DeviceInfo>, Error>,
    },
    /// [`Effect::AdminDo`]'s answer.
    Done(Result<Outcome, Error>),

    /// The Administration button.
    Open,
    /// Back to the signed-in view.
    Close,
    /// Search for the text in the search field.
    Search,
    ClearSearch,
    NextPage,
    PreviousPage,
    Reload,
    NewUser,
    /// Go on from the new account's form to the password.
    SubmitCreate,
    OpenUser(Uuid),
    /// Back to the list from an account's page or the new account's form.
    BackToList,
    /// Go on from the account's role and quota to the password.
    SubmitEdit,
    /// Ask for the password of a change.
    Ask(Action),
    /// Send the change with the password typed.
    Confirm,
    CancelDialog,
    /// The shown code was noted: forget it.
    DismissCode,
    RefreshDevices,
}

impl Model {
    /// The admin pages, if shown.
    pub fn admin_screen(&self) -> Option<&AdminScreen> {
        match &self.screen {
            Screen::Admin(admin) => Some(admin),
            _ => None,
        }
    }

    /// Whether the admin pages are offered: the server said the signed-in account is an admin.
    pub fn is_admin(&self) -> bool {
        self.overview().is_some_and(|o| matches!(o.access, Access::Admin(_)))
    }

    pub(crate) fn admin(&mut self, event: AdminEvent) -> Vec<Effect> {
        match event {
            AdminEvent::Server(result) => self.admin_probed(result),
            AdminEvent::Open => self.open_admin(),
            AdminEvent::Close => {
                self.leave_admin();
                Vec::new()
            }
            event => {
                let Screen::Admin(admin) = &mut self.screen else { return Vec::new() };
                admin.update(event)
            }
        }
    }

    /// The counters came (an admin), or the server said no (a member).
    fn admin_probed(&mut self, result: Result<AdminServerInfo, Error>) -> Vec<Effect> {
        let access = match result {
            Ok(info) => Access::Admin(info),
            Err(Error::Refused { code, .. }) if code == codes::FORBIDDEN => Access::Member,
            // Unreachable, or a server that doesn't know the admin calls: asked again at the next sign-in or refresh.
            Err(_) => return Vec::new(),
        };
        if let Some(overview) = self.overview_mut() {
            overview.access = access.clone();
        }
        if access == Access::Member {
            self.leave_admin();
        }
        Vec::new()
    }

    fn open_admin(&mut self) -> Vec<Effect> {
        let Screen::SignedIn(overview) = &self.screen else { return Vec::new() };
        if !matches!(overview.access, Access::Admin(_)) {
            return Vec::new();
        }
        let mut admin = AdminScreen { overview: overview.clone(), list: UserList::new(), view: View::List, dialog: None, code: None };
        let effects = vec![admin.list.load(), Effect::AdminServer];
        self.screen = Screen::Admin(Box::new(admin));
        effects
    }

    /// Back to the signed-in view; a code still shown, and anything typed, is dropped.
    fn leave_admin(&mut self) {
        if let Screen::Admin(admin) = &self.screen {
            self.screen = Screen::SignedIn(admin.overview.clone());
        }
    }
}

impl AdminScreen {
    fn update(&mut self, event: AdminEvent) -> Vec<Effect> {
        match event {
            AdminEvent::Server(_) | AdminEvent::Open | AdminEvent::Close => Vec::new(),
            AdminEvent::Users { ticket, result } => self.users_came(ticket, result),
            AdminEvent::Devices { user, result } => {
                let mut effects = Vec::new();
                if let View::User(view) = &mut self.view
                    && view.id == user
                {
                    match result {
                        Ok(list) => {
                            view.devices = Some(list);
                            view.devices_error = None;
                        }
                        Err(e) => {
                            view.devices_error = Some(error_text(&e));
                            if is_forbidden(&e) {
                                effects.push(Effect::AdminServer);
                            }
                        }
                    }
                }
                effects
            }
            AdminEvent::Done(result) => self.done(result),
            AdminEvent::Search => {
                self.list.applied = self.list.search.trim().to_owned();
                self.list.search = self.list.applied.clone();
                self.list.cursors = vec![None];
                vec![self.list.load()]
            }
            AdminEvent::ClearSearch => {
                self.list.search.clear();
                self.list.applied.clear();
                self.list.cursors = vec![None];
                vec![self.list.load()]
            }
            AdminEvent::NextPage => match self.list.next.clone() {
                Some(next) if !self.list.loading => {
                    self.list.cursors.push(Some(next));
                    vec![self.list.load()]
                }
                _ => Vec::new(),
            },
            AdminEvent::PreviousPage => {
                if self.list.has_previous() && !self.list.loading {
                    self.list.cursors.pop();
                    vec![self.list.load()]
                } else {
                    Vec::new()
                }
            }
            AdminEvent::Reload => {
                if self.list.loading {
                    return Vec::new();
                }
                vec![self.list.load()]
            }
            AdminEvent::NewUser => {
                if self.dialog.is_none() {
                    self.view = View::Create(CreateForm::default());
                }
                Vec::new()
            }
            AdminEvent::SubmitCreate => self.submit_create(),
            AdminEvent::OpenUser(id) => {
                let Some(user) = self.list.find(id) else { return Vec::new() };
                if self.dialog.is_some() {
                    return Vec::new();
                }
                self.view = View::User(Box::new(UserView::new(user)));
                vec![Effect::AdminDevices(id)]
            }
            AdminEvent::BackToList => {
                if self.dialog.is_none() {
                    self.view = View::List;
                }
                Vec::new()
            }
            AdminEvent::SubmitEdit => {
                self.submit_edit();
                Vec::new()
            }
            AdminEvent::Ask(action) => {
                if self.dialog.is_none() {
                    self.dialog = Some(Dialog { action, confirmation: Confirmation::default(), error: None, busy: false });
                }
                Vec::new()
            }
            AdminEvent::Confirm => self.confirm(),
            AdminEvent::CancelDialog => {
                if self.dialog.as_ref().is_some_and(|d| !d.busy) {
                    self.dialog = None;
                }
                Vec::new()
            }
            AdminEvent::DismissCode => {
                self.code = None;
                Vec::new()
            }
            AdminEvent::RefreshDevices => match &self.view {
                View::User(view) => vec![Effect::AdminDevices(view.id)],
                _ => Vec::new(),
            },
        }
    }

    fn users_came(&mut self, ticket: u64, result: Result<AdminUserList, Error>) -> Vec<Effect> {
        if ticket != self.list.ticket {
            return Vec::new();
        }
        self.list.loading = false;
        match result {
            Ok(list) => {
                self.list.loaded = true;
                self.list.error = None;
                self.list.users = list.users;
                self.list.next = list.next;
                Vec::new()
            }
            Err(e) => {
                self.list.error = Some(error_text(&e));
                if is_forbidden(&e) { vec![Effect::AdminServer] } else { Vec::new() }
            }
        }
    }

    fn submit_create(&mut self) -> Vec<Effect> {
        let View::Create(form) = &mut self.view else { return Vec::new() };
        if self.dialog.is_some() {
            return Vec::new();
        }
        let username = form.username.trim().to_owned();
        if !valid_username(&username) {
            form.error = Some(BAD_USERNAME.to_owned());
            return Vec::new();
        }
        form.error = None;
        let action = Action::CreateUser { username, role: form.role };
        self.dialog = Some(Dialog { action, confirmation: Confirmation::default(), error: None, busy: false });
        Vec::new()
    }

    fn submit_edit(&mut self) {
        let View::User(view) = &mut self.view else { return };
        if self.dialog.is_some() {
            return;
        }
        let Some(user) = self.list.users.iter().find(|u| u.id == view.id) else {
            view.error = Some("That account isn't in the list any more. Go back and look again.".to_owned());
            return;
        };
        let role = (view.role.as_str() != user.role).then_some(view.role);
        let quota_changed = view.quota.trim() != view.quota_shown.trim();
        let quota = if quota_changed {
            match parse_quota(&view.quota) {
                Ok(quota) => Some(quota),
                Err(why) => {
                    view.error = Some(why);
                    return;
                }
            }
        } else {
            None
        };
        if role.is_none() && quota.is_none() {
            view.error = Some("Nothing to change.".to_owned());
            return;
        }
        view.error = None;
        let action = Action::UpdateUser { id: view.id, username: view.username.clone(), role, quota };
        self.dialog = Some(Dialog { action, confirmation: Confirmation::default(), error: None, busy: false });
    }

    fn confirm(&mut self) -> Vec<Effect> {
        let Some(dialog) = &mut self.dialog else { return Vec::new() };
        if dialog.busy {
            return Vec::new();
        }
        if !dialog.confirmation.ready() {
            dialog.error = Some(NEEDS_PASSWORD.to_owned());
            return Vec::new();
        }
        dialog.error = None;
        dialog.busy = true;
        // The typed password leaves the model here: it travels in the call and nowhere else.
        let confirm = dialog.confirmation.take();
        vec![Effect::AdminDo { action: dialog.action.clone(), confirm }]
    }

    fn done(&mut self, result: Result<Outcome, Error>) -> Vec<Effect> {
        let Some(dialog) = &mut self.dialog else { return Vec::new() };
        if !dialog.busy {
            return Vec::new();
        }
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                dialog.busy = false;
                dialog.confirmation = Confirmation::default();
                dialog.error = Some(error_text(&e));
                return if is_forbidden(&e) { vec![Effect::AdminServer] } else { Vec::new() };
            }
        };
        let action = dialog.action.clone();
        self.dialog = None;
        let mut effects = Vec::new();
        match (&action, outcome) {
            (Action::CreateUser { username, .. }, Outcome::Created(created)) => {
                self.show_code(username, CodeKind::Invite, created.invite);
                self.view = View::List;
            }
            (Action::IssueInvite { username, .. }, Outcome::Code(code)) => self.show_code(username, CodeKind::Invite, code),
            (Action::IssueReset { username, .. }, Outcome::Code(code)) => self.show_code(username, CodeKind::PasswordReset, code),
            (Action::UpdateUser { .. }, Outcome::Updated(info)) => {
                if let View::User(view) = &mut self.view
                    && view.id == info.id
                {
                    **view = UserView { devices: view.devices.take(), devices_error: view.devices_error.take(), ..UserView::new(&info) };
                }
            }
            (Action::Remove { .. }, _) => self.view = View::List,
            (Action::RevokeDevice { id, .. }, _) => effects.push(Effect::AdminDevices(*id)),
            _ => {}
        }
        // What changed shows in the list (and the counters).
        effects.push(self.list.load());
        effects.push(Effect::AdminServer);
        effects
    }

    fn show_code(&mut self, username: &str, kind: CodeKind, code: AdminCode) {
        self.code = Some(ShownCode { username: username.to_owned(), kind, code: code.code, join_url: code.join_url, expires_ms: code.expires_ms });
    }
}

/// The words the confirmation page says when no password was typed.
pub const NEEDS_PASSWORD: &str = "Type your own password to confirm";
/// The words when a new account's name breaks the server's rules.
pub const BAD_USERNAME: &str = "A name is 1 to 64 letters, digits, dots, underscores or dashes";

/// Whether `name` is a username the server takes (1-64 ASCII letters, digits, `.`, `_` or `-`).
pub fn valid_username(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn is_forbidden(e: &Error) -> bool {
    matches!(e, Error::Refused { code, .. } if code == codes::FORBIDDEN)
}

/// An admin call's error, for people: the server's refusals the admin can act on in plain words.
pub fn error_text(e: &Error) -> String {
    match e {
        Error::Refused { code, message } => match code.as_str() {
            codes::WRONG_PASSWORD => "That password is wrong. Type your own current password.".to_owned(),
            codes::LAST_ADMIN => "That would leave the server without an enabled admin. Make another account an admin first.".to_owned(),
            codes::ACCOUNT_DISABLED => "Your account is disabled, so the server refuses this.".to_owned(),
            codes::FORBIDDEN => format!("The server doesn't allow this: {}", lower_first(message)),
            _ => message.clone(),
        },
        Error::RefusedWith { code, message, retry_after_secs: Some(secs), .. } if code == codes::RATE_LIMITED => {
            format!("Too many tries. Wait {} and try again. {}", wait(*secs), sentence(message))
        }
        Error::RefusedWith { message, .. } => message.clone(),
        other => other.to_string(),
    }
}

/// "30 seconds", "2 minutes".
fn wait(secs: u64) -> String {
    match secs {
        0..=1 => "a second".to_owned(),
        2..=119 => format!("{secs} seconds"),
        _ => format!("{} minutes", secs.div_ceil(60)),
    }
}

fn lower_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|first| first.to_lowercase().chain(chars).collect()).unwrap_or_default()
}

/// `text` as a sentence: a capital first, a full stop last (nothing for an empty text).
fn sentence(text: &str) -> String {
    let text = text.trim().trim_end_matches('.');
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}.", first.to_uppercase().chain(chars).collect::<String>()),
        None => String::new(),
    }
}

/// A quota typed in GiB (`50`, `0.5`; empty: none).
pub fn parse_quota(text: &str) -> Result<Option<u64>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let bad = || "A quota is a number of GiB, like 50 or 0.5 (empty: no quota)".to_owned();
    let gib: f64 = text.parse().map_err(|_| bad())?;
    if !gib.is_finite() || gib < 0.0 {
        return Err(bad());
    }
    if gib > MAX_QUOTA_GIB {
        return Err(format!("A quota is at most {MAX_QUOTA_GIB:.0} GiB"));
    }
    // At most 2^20 GiB: the product is far under u64's range and rounds to a whole number of bytes.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let bytes = (gib * GIB).round() as u64;
    Ok(Some(bytes))
}

/// A quota's bytes as the quota field shows them: GiB, to 3 places, without trailing zeros.
pub fn quota_text(bytes: u64) -> String {
    let gib = bytes as f64 / GIB;
    let text = format!("{gib:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// A size for people: `512 B`, `1.5 MiB`, `2 GiB`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    let number = if unit == 0 || value >= 100.0 { format!("{value:.0}") } else { format!("{value:.1}") };
    let number = if number.contains('.') { number.trim_end_matches('0').trim_end_matches('.').to_owned() } else { number };
    format!("{number} {}", UNITS.get(unit).copied().unwrap_or("B"))
}

impl From<AdminEvent> for Event {
    fn from(event: AdminEvent) -> Self {
        Event::Admin(event)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agent::fake::{FakeAgent, sample_user};
    use crate::agent::{Starter, call};
    use crate::model::{JoinDefaults, tests::status};

    /// An agent signed in as `fabio`, an admin (or not), with `others` more accounts after fabio's own.
    fn agent(admin: bool, others: usize) -> FakeAgent {
        let agent = FakeAgent::new(status(true));
        agent.admin.store(admin, std::sync::atomic::Ordering::SeqCst);
        let mut users = vec![AdminUserInfo { role: "admin".into(), ..sample_user("fabio", Uuid::now_v7()) }];
        users.extend((1..=others).map(|n| sample_user(&format!("user{n:02}"), Uuid::now_v7())));
        *agent.users.lock().unwrap() = users;
        agent
    }

    /// Feeds `events` to the model, each effect answered by the fake until nothing is left (as the link does).
    fn run(model: &mut Model, agent: &FakeAgent, events: Vec<Event>) {
        let start: Starter = Arc::new(|_: &dyn crate::agent::Agent| Err("not started in tests".to_owned()));
        for event in events {
            let mut pending = model.update(event);
            while let Some(effect) = pending.pop() {
                if let Some(answer) = call(agent, &start, effect) {
                    pending.extend(model.update(answer));
                }
            }
        }
    }

    fn signed_in(agent: &FakeAgent) -> Model {
        let mut model = Model::new(JoinDefaults::default(), true, "run.log".into());
        run(&mut model, agent, vec![Event::Start]);
        model
    }

    /// The model on the admin pages.
    fn on_the_pages(agent: &FakeAgent) -> Model {
        let mut model = signed_in(agent);
        run(&mut model, agent, vec![Event::Admin(AdminEvent::Open)]);
        assert!(model.admin_screen().is_some(), "{:?}", model.screen);
        model
    }

    fn admin(model: &Model) -> &AdminScreen {
        model.admin_screen().unwrap_or_else(|| panic!("not on the admin pages: {:?}", model.screen))
    }

    fn admin_mut(model: &mut Model) -> &mut AdminScreen {
        match &mut model.screen {
            Screen::Admin(admin) => admin,
            other => panic!("not on the admin pages: {other:?}"),
        }
    }

    fn user_named(model: &Model, name: &str) -> AdminUserInfo {
        admin(model).list.users.iter().find(|u| u.username == name).cloned().unwrap_or_else(|| panic!("no {name} in the list"))
    }

    fn ev(event: AdminEvent) -> Vec<Event> {
        vec![Event::Admin(event)]
    }

    fn type_password(model: &mut Model, password: &str) {
        admin_mut(model).dialog.as_mut().unwrap().confirmation.password = password.to_owned();
    }

    fn refused(code: &str, message: &str) -> Error {
        Error::Refused { code: code.into(), message: message.into() }
    }

    /// The pages are for admins: the server's answer to the counters decides, and a member never gets in.
    #[test]
    fn only_an_admin_is_offered_the_pages() {
        let member = agent(false, 3);
        let mut model = signed_in(&member);
        assert_eq!(model.overview().unwrap().access, Access::Member);
        assert!(!model.is_admin());
        assert!(model.update(Event::Admin(AdminEvent::Open)).is_empty());
        assert!(matches!(model.screen, Screen::SignedIn(_)));
        assert_eq!(member.asked.lock().unwrap().iter().filter(|a| *a == "admin_server").count(), 1);

        let boss = agent(true, 3);
        let mut model = signed_in(&boss);
        assert!(model.is_admin());
        let Access::Admin(info) = &model.overview().unwrap().access else { panic!("{:?}", model.screen) };
        assert_eq!((info.users, info.storage_kind.as_str()), (4, "disk"));
        let effects = model.update(Event::Admin(AdminEvent::Open));
        assert!(matches!(effects.as_slice(), [Effect::AdminUsers { .. }, Effect::AdminServer]), "{effects:?}");
        assert!(model.admin_screen().is_some());

        // A server that can't be reached says nothing about the role: no pages, asked again at the next refresh.
        let mut model = Model::new(JoinDefaults::default(), true, String::new());
        model.update(Event::Start);
        model.update(Event::Found(Ok(status(true))));
        model.update(Event::Admin(AdminEvent::Server(Err(Error::Io("reset".into())))));
        assert_eq!(model.overview().unwrap().access, Access::Unknown);
        assert!(model.update(Event::RefreshDevices).contains(&Effect::AdminServer));
    }

    /// An admin who is demoted meanwhile: the server's `forbidden` is checked against the counters, and the pages close.
    #[test]
    fn a_demoted_admin_is_taken_off_the_pages() {
        let boss = agent(true, 2);
        let mut model = on_the_pages(&boss);
        boss.admin.store(false, std::sync::atomic::Ordering::SeqCst);
        run(&mut model, &boss, ev(AdminEvent::Reload));
        assert!(matches!(model.screen, Screen::SignedIn(_)), "{:?}", model.screen);
        assert_eq!(model.overview().unwrap().access, Access::Member);
    }

    /// Pages of 20, a cursor each way, and a search that starts over from the first page; an old answer is ignored.
    #[test]
    fn the_list_is_paged_and_searched() {
        let boss = agent(true, 44);
        let mut model = on_the_pages(&boss);
        assert_eq!(admin(&model).list.users.len(), PAGE_SIZE as usize);
        assert_eq!((admin(&model).list.page(), admin(&model).list.next.as_deref()), (1, Some("20")));
        assert!(model.update(Event::Admin(AdminEvent::PreviousPage)).is_empty(), "no page before the first");
        run(&mut model, &boss, ev(AdminEvent::NextPage));
        assert_eq!(admin(&model).list.page(), 2);
        assert_eq!(admin(&model).list.users[0].username, "user20");
        run(&mut model, &boss, ev(AdminEvent::NextPage));
        assert_eq!((admin(&model).list.users.len(), admin(&model).list.next.clone()), (5, None));
        assert!(model.update(Event::Admin(AdminEvent::NextPage)).is_empty(), "no page after the last");
        run(&mut model, &boss, ev(AdminEvent::PreviousPage));
        assert_eq!((admin(&model).list.page(), admin(&model).list.users[0].username.as_str()), (2, "user20"));

        admin_mut(&mut model).list.search = "  USER4 ".into();
        let effects = model.update(Event::Admin(AdminEvent::Search));
        let [Effect::AdminUsers { query, ticket }] = effects.as_slice() else { panic!("{effects:?}") };
        assert_eq!((query.cursor.as_deref(), query.query.as_deref(), query.limit), (None, Some("USER4"), Some(PAGE_SIZE)));
        let old = ticket - 1;
        model.update(Event::Admin(AdminEvent::Users { ticket: old, result: Ok(AdminUserList { users: Vec::new(), next: None }) }));
        assert!(admin(&model).list.loading, "an answer to an older call is ignored");
        for effect in effects {
            let start: Starter = Arc::new(|_: &dyn crate::agent::Agent| Err(String::new()));
            if let Some(answer) = call(&boss, &start, effect) {
                model.update(answer);
            }
        }
        let names: Vec<&str> = admin(&model).list.users.iter().map(|u| u.username.as_str()).collect();
        assert_eq!(names, ["user40", "user41", "user42", "user43", "user44"]);
        assert_eq!(admin(&model).list.page(), 1);
        run(&mut model, &boss, ev(AdminEvent::ClearSearch));
        assert_eq!((admin(&model).list.applied.as_str(), admin(&model).list.users.len()), ("", PAGE_SIZE as usize));
    }

    /// Every change asks for the password afresh; it leaves the model when it is sent, and shows in no `Debug` output.
    #[test]
    fn a_change_sends_the_password_once_and_clears_it() {
        let boss = agent(true, 3);
        let mut model = on_the_pages(&boss);
        let ana = user_named(&model, "user01");
        run(&mut model, &boss, ev(AdminEvent::OpenUser(ana.id)));
        model.update(Event::Admin(AdminEvent::Ask(Action::Disable { id: ana.id, username: "user01".into() })));
        assert!(admin(&model).dialog.is_some());
        // Nothing typed: nothing is sent.
        assert!(model.update(Event::Admin(AdminEvent::Confirm)).is_empty());
        assert_eq!(admin(&model).dialog.as_ref().unwrap().error.as_deref(), Some(NEEDS_PASSWORD));
        type_password(&mut model, "admin-secret-pw");
        let effects = model.update(Event::Admin(AdminEvent::Confirm));
        let [Effect::AdminDo { action, confirm }] = effects.as_slice() else { panic!("{effects:?}") };
        assert_eq!(action, &Action::Disable { id: ana.id, username: "user01".into() });
        assert_eq!(confirm.password.as_deref(), Some("admin-secret-pw"));
        let dialog = admin(&model).dialog.as_ref().unwrap();
        assert!(dialog.busy && dialog.confirmation.password.is_empty(), "{dialog:?}");
        let shown = format!("{model:?} {effects:?}");
        assert!(!shown.contains("admin-secret-pw"), "{shown}");
        assert!(model.update(Event::Admin(AdminEvent::Confirm)).is_empty(), "not twice");
        assert!(model.update(Event::Admin(AdminEvent::CancelDialog)).is_empty());
        assert!(admin(&model).dialog.is_some(), "not while it is with the server");
        // The call is made with the password, and done: the dialog goes, the list is asked again.
        let start: Starter = Arc::new(|_: &dyn crate::agent::Agent| Err(String::new()));
        let answer = call(&boss, &start, effects.into_iter().next().unwrap()).unwrap();
        assert_eq!(boss.passwords.lock().unwrap().as_slice(), [Some("admin-secret-pw".to_owned())]);
        let effects = model.update(answer);
        assert!(admin(&model).dialog.is_none());
        assert!(effects.iter().any(|e| matches!(e, Effect::AdminUsers { .. })), "{effects:?}");
    }

    /// A wrong password is the server's `wrong_password`: the dialog stays, empty, to try again.
    #[test]
    fn a_wrong_password_keeps_the_dialog_open() {
        let boss = agent(true, 1);
        let mut model = on_the_pages(&boss);
        let ana = user_named(&model, "user01");
        boss.admin_fails.lock().unwrap().push(refused(codes::WRONG_PASSWORD, "the password is wrong"));
        run(
            &mut model,
            &boss,
            vec![
                Event::Admin(AdminEvent::Ask(Action::Remove { id: ana.id, username: "user01".into() })),
                Event::Admin(AdminEvent::Confirm), // nothing typed
            ],
        );
        type_password(&mut model, "not-my-password");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        let dialog = admin(&model).dialog.as_ref().unwrap();
        assert!(!dialog.busy && dialog.confirmation.password.is_empty());
        assert_eq!(dialog.error.as_deref(), Some("That password is wrong. Type your own current password."));
        type_password(&mut model, "my-password");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        assert!(admin(&model).dialog.is_none());
        assert_eq!(boss.passwords.lock().unwrap().iter().flatten().collect::<Vec<_>>(), ["not-my-password", "my-password"]);
    }

    #[test]
    fn the_servers_refusals_are_said_clearly() {
        assert!(error_text(&refused(codes::LAST_ADMIN, "x")).contains("without an enabled admin"));
        assert!(error_text(&refused(codes::ACCOUNT_DISABLED, "x")).contains("disabled"));
        assert_eq!(error_text(&refused(codes::FORBIDDEN, "Only admins can do this")), "The server doesn't allow this: only admins can do this");
        let slow = Error::RefusedWith {
            code: codes::RATE_LIMITED.into(),
            message: "too many wrong passwords; wait a few minutes".into(),
            two_factor: None,
            retry_after_secs: Some(90),
        };
        let text = error_text(&slow);
        assert!(text.contains("Wait 90 seconds") && text.contains("Too many wrong passwords"), "{text}");
        let busy = Error::RefusedWith {
            code: codes::RATE_LIMITED.into(),
            message: "the server is busy".into(),
            two_factor: None,
            retry_after_secs: Some(600),
        };
        assert!(error_text(&busy).contains("Wait 10 minutes"));
        assert_eq!(error_text(&refused("invalid", "role is admin or member")), "role is admin or member");
        assert!(error_text(&Error::NotRunning("x".into())).contains("isn't running"));

        // In the dialog, as the user reads it.
        let boss = agent(true, 1);
        let mut model = on_the_pages(&boss);
        let ana = user_named(&model, "user01");
        for (fail, wanted) in [
            (refused(codes::LAST_ADMIN, "x"), "without an enabled admin"),
            (slow, "Too many tries"),
            (refused(codes::ACCOUNT_DISABLED, "x"), "disabled"),
        ] {
            boss.admin_fails.lock().unwrap().push(fail);
            run(&mut model, &boss, ev(AdminEvent::Ask(Action::Enable { id: ana.id, username: "user01".into() })));
            type_password(&mut model, "pw");
            run(&mut model, &boss, ev(AdminEvent::Confirm));
            let error = admin(&model).dialog.as_ref().unwrap().error.clone().unwrap();
            assert!(error.contains(wanted), "{error}");
            run(&mut model, &boss, ev(AdminEvent::CancelDialog));
            assert!(admin(&model).dialog.is_none());
        }
        // A `forbidden` on a change asks the server again whether this is still an admin.
        boss.admin_fails.lock().unwrap().push(refused(codes::FORBIDDEN, "You can't do that to your own account"));
        run(&mut model, &boss, ev(AdminEvent::Ask(Action::Enable { id: ana.id, username: "user01".into() })));
        type_password(&mut model, "pw");
        let effects = model.update(Event::Admin(AdminEvent::Confirm));
        let start: Starter = Arc::new(|_: &dyn crate::agent::Agent| Err(String::new()));
        let answer = call(&boss, &start, effects.into_iter().next().unwrap()).unwrap();
        assert_eq!(model.update(answer), vec![Effect::AdminServer]);
        assert!(admin(&model).dialog.as_ref().unwrap().error.as_deref().is_some_and(|e| e.starts_with("The server doesn't allow this")));
        assert!(model.admin_screen().is_some(), "still an admin: the pages stay");
    }

    /// A new account has a name and a role, and no quota (that is a change of its own, on its page); the invite it
    /// makes is shown once.
    #[test]
    fn a_new_account_shows_its_invite_once() {
        let boss = agent(true, 1);
        let mut model = on_the_pages(&boss);
        model.update(Event::Admin(AdminEvent::NewUser));
        assert!(matches!(admin(&model).view, View::Create(_)));
        let form = |model: &mut Model| match &mut admin_mut(model).view {
            View::Create(form) => form.clone(),
            other => panic!("{other:?}"),
        };
        let long = "x".repeat(65);
        for bad in ["", "   ", "a b", "ana/../root", long.as_str()] {
            if let View::Create(f) = &mut admin_mut(&mut model).view {
                f.username = bad.into();
            }
            assert!(model.update(Event::Admin(AdminEvent::SubmitCreate)).is_empty());
            assert_eq!(form(&mut model).error.as_deref(), Some(BAD_USERNAME), "{bad:?}");
            assert!(admin(&model).dialog.is_none());
        }
        if let View::Create(f) = &mut admin_mut(&mut model).view {
            f.username = " new.user_1 ".into();
            f.role = RoleChoice::Admin;
        }
        model.update(Event::Admin(AdminEvent::SubmitCreate));
        let Some(Dialog { action, .. }) = &admin(&model).dialog else { panic!("no dialog") };
        assert_eq!(action, &Action::CreateUser { username: "new.user_1".into(), role: RoleChoice::Admin });
        type_password(&mut model, "admin-secret-pw");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        assert_eq!(boss.asked.lock().unwrap().iter().filter(|a| *a == "admin_create_user").count(), 1);
        let screen = admin(&model);
        assert!(screen.dialog.is_none() && matches!(screen.view, View::List));
        let code = screen.code.as_ref().unwrap();
        assert_eq!((code.username.as_str(), code.kind, code.code.as_str()), ("new.user_1", CodeKind::Invite, "K7M4Q9TXK7M4Q9TX"));
        let shown = format!("{model:?}");
        assert!(!shown.contains("K7M4Q9TX") && !shown.contains("K7M4-Q9TX") && !shown.contains("admin-secret-pw"), "{shown}");
        // Dismissed, it is gone from the model; leaving the pages drops it too.
        model.update(Event::Admin(AdminEvent::DismissCode));
        assert!(admin(&model).code.is_none());
        assert_eq!(format!("{:?}", admin(&model).list.users).matches("K7M4").count(), 0);
        model.update(Event::Admin(AdminEvent::Close));
        assert!(matches!(model.screen, Screen::SignedIn(_)));
    }

    /// Role and quota are one change, with only what differs sent; an empty quota removes it.
    #[test]
    fn an_account_page_changes_the_role_and_the_quota() {
        let boss = agent(true, 2);
        let mut model = on_the_pages(&boss);
        let ana = AdminUserInfo { quota_bytes: Some(5 * 1_073_741_824), ..user_named(&model, "user01") };
        boss.users.lock().unwrap()[1] = ana.clone();
        run(&mut model, &boss, ev(AdminEvent::Reload));
        run(&mut model, &boss, ev(AdminEvent::OpenUser(ana.id)));
        let view = |model: &Model| match &admin(model).view {
            View::User(view) => (**view).clone(),
            other => panic!("{other:?}"),
        };
        let v = view(&model);
        assert_eq!((v.role, v.quota.as_str(), v.devices.as_ref().map(Vec::len)), (RoleChoice::Member, "5", Some(1)));
        // Nothing changed: nothing to ask.
        model.update(Event::Admin(AdminEvent::SubmitEdit));
        assert_eq!(view(&model).error.as_deref(), Some("Nothing to change."));
        let set = |model: &mut Model, role: RoleChoice, quota: &str| {
            if let View::User(v) = &mut admin_mut(model).view {
                v.role = role;
                v.quota = quota.into();
            }
        };
        set(&mut model, RoleChoice::Member, "lots");
        model.update(Event::Admin(AdminEvent::SubmitEdit));
        assert!(view(&model).error.unwrap().starts_with("A quota is a number of GiB"));
        assert!(admin(&model).dialog.is_none());
        set(&mut model, RoleChoice::Admin, "5");
        model.update(Event::Admin(AdminEvent::SubmitEdit));
        let Some(Dialog { action, .. }) = &admin(&model).dialog else { panic!("no dialog") };
        assert_eq!(action, &Action::UpdateUser { id: ana.id, username: "user01".into(), role: Some(RoleChoice::Admin), quota: None });
        run(&mut model, &boss, ev(AdminEvent::CancelDialog));
        set(&mut model, RoleChoice::Member, "");
        model.update(Event::Admin(AdminEvent::SubmitEdit));
        let Some(Dialog { action, .. }) = &admin(&model).dialog else { panic!("no dialog") };
        assert_eq!(action, &Action::UpdateUser { id: ana.id, username: "user01".into(), role: None, quota: Some(None) });
        assert!(action.explain().contains("quota is removed"));
        type_password(&mut model, "pw");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        let v = view(&model);
        assert_eq!((v.quota.as_str(), v.error), ("", None), "the page shows what the server answered");
        assert_eq!(v.devices.map(|d| d.len()), Some(1), "the devices stay");
    }

    /// An invite and a reset code are shown once, each; a revoke and a device revoke refresh what they changed.
    #[test]
    fn invites_reset_codes_and_devices() {
        let boss = agent(true, 1);
        let mut model = on_the_pages(&boss);
        let ana = user_named(&model, "user01");
        run(&mut model, &boss, ev(AdminEvent::OpenUser(ana.id)));
        for (action, kind, code) in [
            (Action::IssueInvite { id: ana.id, username: "user01".into() }, CodeKind::Invite, "ABCDEFGHJKMNPQRS"),
            (Action::IssueReset { id: ana.id, username: "user01".into() }, CodeKind::PasswordReset, "TUVWXYZ234567ABC"),
        ] {
            run(&mut model, &boss, ev(AdminEvent::Ask(action)));
            type_password(&mut model, "pw");
            run(&mut model, &boss, ev(AdminEvent::Confirm));
            let shown = admin(&model).code.clone().unwrap();
            assert_eq!((shown.kind, shown.code.as_str()), (kind, code));
            assert!(matches!(admin(&model).view, View::User(_)), "still on the account's page");
            run(&mut model, &boss, ev(AdminEvent::DismissCode));
        }
        run(&mut model, &boss, ev(AdminEvent::Ask(Action::RevokeInvite { id: ana.id, username: "user01".into() })));
        type_password(&mut model, "pw");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        assert!(admin(&model).code.is_none() && admin(&model).dialog.is_none());

        let View::User(view) = &admin(&model).view else { panic!() };
        let device = view.devices.as_ref().unwrap()[0].clone();
        let before = boss.asked.lock().unwrap().iter().filter(|a| *a == "admin_devices").count();
        let revoke = Action::RevokeDevice { id: ana.id, username: "user01".into(), device: device.id, device_name: device.name.clone() };
        assert!(revoke.title().contains("laptop"));
        run(&mut model, &boss, ev(AdminEvent::Ask(revoke)));
        type_password(&mut model, "pw");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        assert_eq!(boss.asked.lock().unwrap().iter().filter(|a| *a == "admin_devices").count(), before + 1, "the devices are asked again");

        // Removing the account goes back to the list.
        run(&mut model, &boss, ev(AdminEvent::Ask(Action::Remove { id: ana.id, username: "user01".into() })));
        type_password(&mut model, "pw");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        assert!(matches!(admin(&model).view, View::List));
    }

    #[test]
    fn quotas_are_typed_in_gib_and_shown_in_the_largest_unit() {
        assert_eq!(parse_quota(""), Ok(None));
        assert_eq!(parse_quota(" 50 "), Ok(Some(50 * 1_073_741_824)));
        assert_eq!(parse_quota("0.5"), Ok(Some(536_870_912)));
        assert_eq!(parse_quota("0"), Ok(Some(0)));
        for bad in ["-1", "abc", "inf", "NaN", "1e99", "2000000", "5 GB"] {
            assert!(parse_quota(bad).is_err(), "{bad}");
        }
        assert_eq!(quota_text(5 * 1_073_741_824), "5");
        assert_eq!(quota_text(536_870_912), "0.5");
        assert_eq!(quota_text(0), "0");
        assert_eq!(parse_quota(&quota_text(1_610_612_736)), Ok(Some(1_610_612_736)));
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1_536), "1.5 KiB");
        assert_eq!(format_bytes(5_000_000_000), "4.7 GiB");
        assert_eq!(format_bytes(2 * 1_073_741_824), "2 GiB");
        assert!(valid_username("a.b_c-9") && !valid_username("") && !valid_username("a b") && !valid_username("é"));
    }

    #[test]
    fn nothing_secret_is_in_debug_output() {
        let confirmation = Confirmation { password: "admin-secret-pw".into() };
        assert!(!format!("{confirmation:?}").contains("admin-secret-pw"));
        let code = ShownCode {
            username: "ana".into(),
            kind: CodeKind::Invite,
            code: "ABCDEFGH".into(),
            join_url: Some("c.example/ABCD".into()),
            expires_ms: 1,
        };
        let shown = format!("{code:?}");
        assert!(!shown.contains("ABCDEFGH") && !shown.contains("c.example"), "{shown}");
    }
}
