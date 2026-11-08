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

use craft_cloud_app::{
    AccountInfo, AccountUsage, AdminCode, AdminCompression, AdminCreatedUser, AdminServerInfo, AdminStorageOverview, AdminUserInfo, AdminUserList,
    DeviceId, DeviceInfo, Error, UserQuery, Uuid,
};
use craft_cloud_proto::api::{AdminConfirm, codes};

use crate::model::{Effect, Event, Model, Overview, Screen};
use crate::security::{digits, explain, is_code};

/// A number no earlier call of the admin pages had, for matching an answer to its call: the numbers never restart, so an
/// answer that comes late (after the pages were left and opened again, or the account signed out and in) matches
/// nothing and is ignored.
pub(crate) fn fresh() -> u64 {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    LAST.fetch_add(1, std::sync::atomic::Ordering::Relaxed).wrapping_add(1)
}

/// Accounts on one page of the list.
pub const PAGE_SIZE: u32 = 20;
/// The most a quota may be, in GiB (a petabyte: a typo, not a quota, past it).
pub const MAX_QUOTA_GIB: f64 = 1_048_576.0;
const GIB: f64 = 1_073_741_824.0;

/// Whether the signed-in account is an admin, as the server describes the account (`GET /account`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Access {
    /// Not asked yet, or the server couldn't be reached: no admin pages.
    #[default]
    Unknown,
    /// The account is a member (or the server refused the admin calls as `forbidden`).
    Member,
    /// The server says the account is an admin: the admin pages are offered. `two_factor`: whether the account has two-factor
    /// sign-in, so that its confirmations ask for a code too.
    Admin { two_factor: bool },
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

/// The admin's confirmation of a change: their current password and, when their account has two-factor sign-in, a code from
/// the authenticator app or a recovery code (one field: six digits are a code, anything else a recovery code, as on the Security
/// page). Its `Debug` output hides both.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Confirmation {
    pub password: String,
    pub code: String,
}

impl std::fmt::Debug for Confirmation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Confirmation").field("password", &"<redacted>").field("code", &"<redacted>").finish()
    }
}

impl Confirmation {
    /// Whether enough was typed to send: the password, and the code where the page asks for one.
    pub fn ready(&self, needs_code: bool) -> bool {
        !self.password.is_empty() && (!needs_code || !self.code.trim().is_empty())
    }

    /// The confirmation as the calls send it, with everything typed cleared from here.
    pub fn take(&mut self) -> AdminConfirm {
        let password = std::mem::take(&mut self.password);
        let code = std::mem::take(&mut self.code);
        let code = code.trim();
        let (totp_code, recovery_code) = if code.is_empty() {
            (None, None)
        } else if is_code(code) {
            (Some(digits(code)), None)
        } else {
            (None, Some(code.to_owned()))
        };
        AdminConfirm { password: Some(password), totp_code, recovery_code }
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
    /// A password reset code; `reset_two_factor`: redeeming it also turns the account's two-factor sign-in off.
    IssueReset {
        id: Uuid,
        username: String,
        reset_two_factor: bool,
    },
    /// Takes the account's two-factor sign-in away and signs its computers out.
    ResetTwoFactor {
        id: Uuid,
        username: String,
    },
    RevokeDevice {
        id: Uuid,
        username: String,
        device: DeviceId,
        device_name: String,
    },
    /// The zstd level the server compresses new writes at (1 to 19).
    SetCompression {
        level: u8,
    },
}

/// The lowest and highest compression level the server takes.
pub const LEVELS: std::ops::RangeInclusive<u8> = 1..=19;

impl Action {
    /// The account the change is about (the new account's name for a creation); empty for a change of the server.
    pub fn username(&self) -> &str {
        match self {
            Self::SetCompression { .. } => "",
            Self::CreateUser { username, .. }
            | Self::UpdateUser { username, .. }
            | Self::Disable { username, .. }
            | Self::Enable { username, .. }
            | Self::Remove { username, .. }
            | Self::IssueInvite { username, .. }
            | Self::RevokeInvite { username, .. }
            | Self::IssueReset { username, .. }
            | Self::ResetTwoFactor { username, .. }
            | Self::RevokeDevice { username, .. } => username,
        }
    }

    /// Whether the change ends or removes something: its confirming button is the red outlined one.
    pub fn destructive(&self) -> bool {
        matches!(
            self,
            Self::Disable { .. } | Self::Remove { .. } | Self::RevokeInvite { .. } | Self::ResetTwoFactor { .. } | Self::RevokeDevice { .. }
        )
    }

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
            Self::ResetTwoFactor { username, .. } => format!("Turn two-factor sign-in off for {username}"),
            Self::RevokeDevice { username, device_name, .. } => format!("Sign {device_name} of {username} out"),
            Self::SetCompression { level } => format!("Compress new files at level {level}"),
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
            Self::IssueReset { reset_two_factor, .. } => {
                let mut text = "A code good for an hour that sets a new password. Using it signs every computer of the account out. Shown once.".to_owned();
                if *reset_two_factor {
                    text.push_str(" Using it also turns the account's two-factor sign-in off.");
                }
                text
            }
            Self::ResetTwoFactor { .. } => {
                "The account no longer asks for a code at sign-in, and every computer of it is signed out. It can turn it on again on its Security page.".into()
            }
            Self::RevokeDevice { .. } => "That computer can't sync any more; its files stay there.".into(),
            Self::SetCompression { .. } => {
                "New files are stored at this level at once and it is kept over restarts. Files already stored are not rewritten. A higher level \
                 saves more space and takes more time on the server."
                    .into()
            }
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
            Self::ResetTwoFactor { .. } => "Turn two-factor off",
            Self::RevokeDevice { .. } => "Sign out",
            Self::SetCompression { .. } => "Set the level",
        }
    }
}

/// What a change came to.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Created(AdminCreatedUser),
    Updated(AdminUserInfo),
    Code(AdminCode),
    /// The compression part of the storage overview after a level change.
    Compression(Box<AdminCompression>),
    Done,
}

/// The password page of one change.
#[derive(Debug, Clone, PartialEq)]
pub struct Dialog {
    pub action: Action,
    pub confirmation: Confirmation,
    /// Whether the page asks for a code too (the admin's account has two-factor sign-in, or the server said so).
    pub needs_code: bool,
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
        self.ticket = fresh();
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
    /// What the role field showed when the page was opened or last saved, to tell whether it was changed.
    pub role_shown: RoleChoice,
    /// What the quota field showed when the page was opened or last saved, to tell whether it was changed.
    pub quota_shown: String,
    pub error: Option<String>,
    pub devices: Option<Vec<DeviceInfo>>,
    pub devices_error: Option<String>,
    /// The number of the devices call whose answer is wanted.
    devices_ticket: u64,
    pub usage: Option<AccountUsage>,
    pub usage_error: Option<String>,
    pub usage_loading: bool,
    usage_ticket: u64,
}

impl UserView {
    fn new(user: &AdminUserInfo) -> Self {
        let quota = user.quota_bytes.map(quota_text).unwrap_or_default();
        Self {
            id: user.id,
            username: user.username.clone(),
            role: RoleChoice::of(&user.role),
            role_shown: RoleChoice::of(&user.role),
            quota_shown: quota.clone(),
            quota,
            error: None,
            devices: None,
            devices_error: None,
            devices_ticket: 0,
            usage: None,
            usage_error: None,
            usage_loading: false,
            usage_ticket: 0,
        }
    }

    /// Whether the role or the quota field holds a change that was not saved.
    pub fn is_dirty(&self) -> bool {
        self.role != self.role_shown || self.quota.trim() != self.quota_shown.trim()
    }

    /// The fields as they were when the page was opened or last saved: what Discard does.
    fn discard_edits(&mut self) {
        self.role = self.role_shown;
        self.quota = self.quota_shown.clone();
        self.error = None;
    }
}

/// Which page of the admin pages is shown.
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    List,
    Create(CreateForm),
    User(Box<UserView>),
    /// The security audit log ([`crate::audit`]).
    Audit(Box<crate::audit::AuditView>),
}

/// Which part of the admin landing page is shown: the accounts, or the server's storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Section {
    #[default]
    Accounts,
    Storage,
}

/// The admin pages.
#[derive(Debug, Clone, PartialEq)]
pub struct AdminScreen {
    /// The signed-in view the pages came from and go back to.
    pub overview: Overview,
    pub list: UserList,
    /// The server's counters, once they came.
    pub server: Option<AdminServerInfo>,
    pub storage: Option<AdminStorageOverview>,
    pub storage_loading: bool,
    pub storage_error: Option<String>,
    storage_ticket: u64,
    /// The compression level picked on the Storage part, before it is confirmed (`None`: the level in use).
    pub level_pick: Option<u8>,
    pub view: View,
    /// The part of the list page that is shown.
    pub section: Section,
    /// A change waiting for the admin's password.
    pub dialog: Option<Dialog>,
    /// A code just made, until it is dismissed.
    pub code: Option<ShownCode>,
    /// What the admin asked for while an account's page held unsaved edits, until Discard or Keep editing answers it.
    pub leaving: Option<Box<Event>>,
}

impl AdminScreen {
    /// Whether an account's page holds role or quota edits that were not saved. While a save is under way the fields are
    /// the ones being sent, so they don't count.
    pub fn has_unsaved_edits(&self) -> bool {
        self.dialog.as_ref().is_none_or(|d| !d.busy) && matches!(&self.view, View::User(view) if view.is_dirty())
    }
}

/// What happened on the admin pages: clicks and the answers of the admin calls.
#[derive(Debug, Clone, PartialEq)]
pub enum AdminEvent {
    /// [`Effect::Account`]'s answer: the role of the signed-in account.
    Account {
        session: u64,
        result: Result<AccountInfo, Error>,
    },
    /// [`Effect::AdminServer`]'s answer: the counters.
    Server {
        session: u64,
        result: Result<AdminServerInfo, Error>,
    },
    /// [`Effect::AdminStorage`]'s answer.
    Storage {
        session: u64,
        ticket: u64,
        result: Result<AdminStorageOverview, Error>,
    },
    /// [`Effect::AdminUsers`]'s answer.
    Users {
        ticket: u64,
        result: Result<AdminUserList, Error>,
    },
    /// [`Effect::AdminDevices`]'s answer.
    Devices {
        user: Uuid,
        ticket: u64,
        result: Result<Vec<DeviceInfo>, Error>,
    },
    /// [`Effect::AdminUserUsage`]'s answer.
    Usage {
        user: Uuid,
        ticket: u64,
        result: Result<AccountUsage, Error>,
    },
    /// [`Effect::AdminDo`]'s answer.
    Done(Result<Outcome, Error>),

    /// The Accounts or Storage tab of the list page.
    Show(Section),
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
    /// The Storage part's compression level control.
    PickLevel(u8),
    /// Ask for the password of a change.
    Ask(Action),
    /// Send the change with the password typed.
    Confirm,
    CancelDialog,
    /// The shown code was noted: forget it.
    DismissCode,
    RefreshDevices,
    RefreshUsage,
    /// "Discard" on the question about unsaved edits: the fields go back to what was saved and what was asked for goes on.
    DiscardEdits,
    /// "Keep editing": what was asked for is dropped.
    KeepEditing,
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
        match self.overview() {
            Some(o) => matches!(o.access, Access::Admin { .. }),
            // Versions and trash don't carry the signed-in view: the last check says.
            None => matches!(self.screen, Screen::Versions(_) | Screen::Trash(_)) && matches!(self.access, Access::Admin { .. }),
        }
    }

    pub(crate) fn admin(&mut self, event: AdminEvent) -> Vec<Effect> {
        match event {
            AdminEvent::Account { session, result } => self.admin_probed(session, result),
            AdminEvent::Open => self.open_admin(),
            AdminEvent::Close => {
                self.leave_admin();
                Vec::new()
            }
            AdminEvent::DiscardEdits => {
                let Screen::Admin(admin) = &mut self.screen else { return Vec::new() };
                let Some(asked) = admin.leaving.take() else { return Vec::new() };
                if let View::User(view) = &mut admin.view {
                    view.discard_edits();
                }
                self.update(*asked)
            }
            event => {
                let Screen::Admin(admin) = &mut self.screen else { return Vec::new() };
                admin.update(event)
            }
        }
    }

    /// The account came (an admin or not), or its role can't be told.
    fn admin_probed(&mut self, session: u64, result: Result<AccountInfo, Error>) -> Vec<Effect> {
        // An answer for an earlier sign-in (another account, maybe) says nothing about this one.
        if self.overview().map(|o| o.session) != Some(session) {
            return Vec::new();
        }
        if let Ok(account) = &result {
            self.names.learn_account(account.user_id, &account.username);
        }
        let access = match result {
            Ok(account) if account.role == "admin" => Access::Admin { two_factor: account.two_factor_enabled },
            Ok(_) => Access::Member,
            Err(Error::Refused { code, .. }) if code == codes::FORBIDDEN => Access::Member,
            // Unreachable, or an agent or server that doesn't know the call: asked again at the next sign-in or refresh.
            Err(_) => return Vec::new(),
        };
        if let Some(overview) = self.overview_mut() {
            overview.access = access.clone();
        }
        self.access = access.clone();
        if access == Access::Member {
            self.leave_admin();
        }
        Vec::new()
    }

    fn open_admin(&mut self) -> Vec<Effect> {
        let Screen::SignedIn(overview) = &self.screen else { return Vec::new() };
        if !matches!(overview.access, Access::Admin { .. }) {
            return Vec::new();
        }
        let mut admin = AdminScreen {
            overview: overview.clone(),
            list: UserList::new(),
            server: None,
            storage: None,
            storage_loading: false,
            storage_error: None,
            storage_ticket: 0,
            level_pick: None,
            view: View::List,
            section: Section::Accounts,
            dialog: None,
            code: None,
            leaving: None,
        };
        let effects = vec![admin.list.load(), Effect::AdminServer { session: admin.overview.session }, admin.load_storage()];
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
            AdminEvent::Account { .. } | AdminEvent::Open | AdminEvent::Close | AdminEvent::DiscardEdits => Vec::new(),
            AdminEvent::KeepEditing => {
                self.leaving = None;
                Vec::new()
            }
            AdminEvent::Show(section) => {
                self.section = section;
                // The audit log opens inside these pages with the switcher over it: a segment leaves it.
                if matches!(self.view, View::Audit(_)) && self.dialog.is_none() {
                    self.view = View::List;
                }
                Vec::new()
            }
            AdminEvent::Server { session, result } => {
                if session != self.overview.session {
                    return Vec::new();
                }
                match result {
                    Ok(info) => {
                        self.server = Some(info);
                        Vec::new()
                    }
                    // Not an admin any more, or not told: the account is looked at again.
                    Err(e) if is_forbidden(&e) => vec![Effect::Account { session: self.overview.session }],
                    Err(_) => Vec::new(),
                }
            }
            AdminEvent::Storage { session, ticket, result } => {
                if session != self.overview.session || ticket != self.storage_ticket {
                    return Vec::new();
                }
                self.storage_loading = false;
                match result {
                    Ok(storage) => {
                        self.storage = Some(storage);
                        self.storage_error = None;
                        Vec::new()
                    }
                    Err(error) => {
                        self.storage_error = Some(error_text(&error));
                        if is_forbidden(&error) { vec![Effect::Account { session: self.overview.session }] } else { Vec::new() }
                    }
                }
            }
            AdminEvent::Users { ticket, result } => self.users_came(ticket, result),
            AdminEvent::Devices { user, ticket, result } => {
                let mut effects = Vec::new();
                if let View::User(view) = &mut self.view
                    && view.id == user
                    && view.devices_ticket == ticket
                {
                    match result {
                        Ok(list) => {
                            view.devices = Some(list);
                            view.devices_error = None;
                        }
                        Err(e) => {
                            view.devices_error = Some(error_text(&e));
                            if is_forbidden(&e) {
                                effects.push(Effect::Account { session: self.overview.session });
                            }
                        }
                    }
                }
                effects
            }
            AdminEvent::Usage { user, ticket, result } => {
                let mut effects = Vec::new();
                if let View::User(view) = &mut self.view
                    && view.id == user
                    && view.usage_ticket == ticket
                {
                    view.usage_loading = false;
                    match result {
                        Ok(usage) => {
                            view.usage = Some(usage);
                            view.usage_error = None;
                        }
                        Err(error) => {
                            view.usage_error = Some(error_text(&error));
                            if is_forbidden(&error) {
                                effects.push(Effect::Account { session: self.overview.session });
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
                vec![self.list.load(), self.load_storage()]
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
                let mut view = UserView::new(user);
                view.devices_ticket = fresh();
                view.usage_ticket = fresh();
                view.usage_loading = true;
                let effects = vec![
                    Effect::AdminDevices { user: id, ticket: view.devices_ticket },
                    Effect::AdminUserUsage { user: id, ticket: view.usage_ticket },
                ];
                self.view = View::User(Box::new(view));
                effects
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
            AdminEvent::PickLevel(level) => {
                if LEVELS.contains(&level) {
                    self.level_pick = Some(level);
                }
                Vec::new()
            }
            AdminEvent::Ask(action) => {
                if self.dialog.is_none() {
                    self.dialog = Some(self.dialog(action));
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
            AdminEvent::RefreshDevices => match &mut self.view {
                View::User(view) => {
                    view.devices_ticket = fresh();
                    vec![Effect::AdminDevices { user: view.id, ticket: view.devices_ticket }]
                }
                _ => Vec::new(),
            },
            AdminEvent::RefreshUsage => match &mut self.view {
                View::User(view) => {
                    view.usage_ticket = fresh();
                    view.usage_loading = true;
                    view.usage_error = None;
                    vec![Effect::AdminUserUsage { user: view.id, ticket: view.usage_ticket }]
                }
                _ => Vec::new(),
            },
        }
    }

    fn load_storage(&mut self) -> Effect {
        self.storage_ticket = fresh();
        self.storage_loading = true;
        self.storage_error = None;
        Effect::AdminStorage { session: self.overview.session, ticket: self.storage_ticket }
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
                if is_forbidden(&e) { vec![Effect::Account { session: self.overview.session }] } else { Vec::new() }
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
        self.dialog = Some(self.dialog(action));
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
        self.dialog = Some(self.dialog(action));
    }

    fn confirm(&mut self) -> Vec<Effect> {
        let Some(dialog) = &mut self.dialog else { return Vec::new() };
        if dialog.busy {
            return Vec::new();
        }
        if dialog.confirmation.password.is_empty() {
            dialog.error = Some(NEEDS_PASSWORD.to_owned());
            return Vec::new();
        }
        if !dialog.confirmation.ready(dialog.needs_code) {
            dialog.error = Some(NEEDS_CODE.to_owned());
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
                // The server wants a code: ask for it (the account's two-factor state may have changed since sign-in).
                if e.code() == Some(codes::TWO_FACTOR_INVALID) {
                    dialog.needs_code = true;
                }
                dialog.error = Some(match (&dialog.action, &e) {
                    (Action::RevokeInvite { .. }, Error::Refused { code, .. }) if code == codes::NOT_FOUND => NO_OPEN_CODE.to_owned(),
                    _ => error_text(&e),
                });
                let none_open = matches!(&dialog.action, Action::RevokeInvite { .. }) && dialog.error.as_deref() == Some(NO_OPEN_CODE);
                return if is_forbidden(&e) {
                    vec![Effect::Account { session: self.overview.session }]
                } else if none_open {
                    // The list said an invite was open, or didn't say: look again.
                    vec![self.list.load()]
                } else {
                    Vec::new()
                };
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
                    **view = UserView {
                        devices: view.devices.take(),
                        devices_error: view.devices_error.take(),
                        devices_ticket: view.devices_ticket,
                        ..UserView::new(&info)
                    };
                    view.usage_ticket = fresh();
                    view.usage_loading = true;
                    effects.push(Effect::AdminUserUsage { user: view.id, ticket: view.usage_ticket });
                }
            }
            (Action::SetCompression { .. }, Outcome::Compression(compression)) => {
                // The answer is the new compression part: shown at once, and the overview is asked for again below.
                if let Some(storage) = &mut self.storage {
                    storage.compression = Some(compression);
                }
                self.level_pick = None;
            }
            (Action::Remove { .. }, _) => self.view = View::List,
            (Action::RevokeDevice { id, .. }, _) => {
                if let View::User(view) = &mut self.view
                    && view.id == *id
                {
                    view.devices_ticket = fresh();
                    effects.push(Effect::AdminDevices { user: *id, ticket: view.devices_ticket });
                }
            }
            _ => {}
        }
        // What changed shows in the list (and the counters).
        effects.push(self.list.load());
        effects.push(Effect::AdminServer { session: self.overview.session });
        effects.push(self.load_storage());
        effects
    }

    /// A page for the confirmation of `action`: it asks for a code too when the account has two-factor sign-in.
    fn dialog(&self, action: Action) -> Dialog {
        let needs_code = matches!(self.overview.access, Access::Admin { two_factor: true });
        Dialog { action, confirmation: Confirmation::default(), needs_code, error: None, busy: false }
    }

    fn show_code(&mut self, username: &str, kind: CodeKind, code: AdminCode) {
        self.code = Some(ShownCode { username: username.to_owned(), kind, code: code.code, join_url: code.join_url, expires_ms: code.expires_ms });
    }
}

/// The words when a revoke finds no invite or reset code open (the server's `not_found`).
pub const NO_OPEN_CODE: &str = "This account has no open invite or password reset code.";
/// The words the confirmation page says when the code is missing.
pub const NEEDS_CODE: &str = "Type the code from your authenticator app (or a recovery code) to confirm";
/// The words the confirmation page says when no password was typed.
pub const NEEDS_PASSWORD: &str = "Type your own password to confirm";
/// The words when a new account's name breaks the server's rules.
pub const BAD_USERNAME: &str = "A name is 1 to 64 letters, digits, dots, underscores or dashes";

/// Whether `name` is a username the server takes (1-64 ASCII letters, digits, `.`, `_` or `-`).
pub fn valid_username(name: &str) -> bool {
    (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

pub(crate) fn is_forbidden(e: &Error) -> bool {
    matches!(e, Error::Refused { code, .. } if code == codes::FORBIDDEN)
}

/// An admin call's error, for people: the server's refusals the admin can act on in plain words (a wrong password, a wrong or
/// locked second factor and a rate limit as the Security page says them).
pub fn error_text(e: &Error) -> String {
    match e.code() {
        Some(codes::WRONG_PASSWORD | codes::TWO_FACTOR_INVALID | codes::TWO_FACTOR_LOCKED | codes::RATE_LIMITED) => explain(e),
        Some(codes::LAST_ADMIN) => "That would leave the server without an enabled admin. Make another account an admin first.".to_owned(),
        Some(codes::ACCOUNT_DISABLED) => "Your account is disabled, so the server refuses this.".to_owned(),
        Some(codes::FORBIDDEN) => match e {
            Error::Refused { message, .. } | Error::RefusedWith { message, .. } => format!("The server doesn't allow this: {}", lower_first(message)),
            other => other.to_string(),
        },
        _ => match e {
            Error::Refused { message, .. } | Error::RefusedWith { message, .. } => message.clone(),
            other => other.to_string(),
        },
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

/// The share of a store, a disk or the capacity, in percent, from which the pages warn. It is the server's own line for an
/// account that is near its quota (`accounts_near_quota`).
pub const WARN_PERCENT: u64 = 90;

/// How much of something is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Room {
    pub used: u64,
    pub total: u64,
}

impl Room {
    /// The share used, in whole percent (0 for a room without a size); it can pass 100.
    pub fn percent(self) -> u64 {
        if self.total == 0 {
            return 0;
        }
        u64::try_from(u128::from(self.used) * 100 / u128::from(self.total)).unwrap_or(u64::MAX)
    }

    /// The share used, 0.0 to 1.0, for a bar.
    pub fn fraction(self) -> f32 {
        if self.total == 0 { 0.0 } else { (self.used as f64 / self.total as f64).clamp(0.0, 1.0) as f32 }
    }

    /// Whether at least [`WARN_PERCENT`] is used. A room without a size is never near.
    pub fn near(self) -> bool {
        self.total > 0 && u128::from(self.used) * 100 >= u128::from(self.total) * u128::from(WARN_PERCENT)
    }

    /// Whether all of it is used.
    pub fn full(self) -> bool {
        self.total > 0 && self.used >= self.total
    }
}

/// What a store's room is measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The bytes the store's filesystem has used, of its size.
    Disk,
    /// What the accounts have stored, of the capacity the admin set.
    Capacity,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Capacity => "capacity",
        }
    }
}

/// A room at or past the warning share, and what it is a room of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Warning {
    pub source: Source,
    pub room: Room,
}

impl Warning {
    /// "disk 92 %", "capacity 120 %".
    pub fn text(self) -> String {
        format!("{} {} %", self.source.label(), self.room.percent())
    }
}

/// What the overview says about the one store the server has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreRoom {
    /// A disk store: the used and total bytes of its filesystem, and, if the admin set a capacity, what the accounts have
    /// stored of it.
    Filesystem { kind: String, room: Room, capacity: Option<Room> },
    /// A store that has only the capacity the admin set (S3), or a disk whose filesystem could not be read: what the accounts
    /// have stored of it.
    Capacity { kind: String, room: Room },
    /// A store with no size to show (S3 or memory without `capacity` in `[storage]`).
    NoCapacity { kind: String },
    /// A disk store the server sent no numbers for.
    NoNumbers { kind: String },
    /// An older server that doesn't say what its store is.
    Unknown,
}

impl StoreRoom {
    /// The room that is measured, if there is one: the filesystem's for a disk, else the capacity's.
    pub fn room(&self) -> Option<Room> {
        match self {
            Self::Filesystem { room, .. } | Self::Capacity { room, .. } => Some(*room),
            Self::NoCapacity { .. } | Self::NoNumbers { .. } | Self::Unknown => None,
        }
    }

    /// The kind the server named, if it did.
    pub fn kind(&self) -> Option<&str> {
        match self {
            Self::Filesystem { kind, .. } | Self::Capacity { kind, .. } | Self::NoCapacity { kind } | Self::NoNumbers { kind } => {
                Some(kind.as_str()).filter(|kind| !kind.is_empty())
            }
            Self::Unknown => None,
        }
    }

    /// The rooms at or past the warning share: a disk's filesystem, and the stored bytes against a capacity.
    pub fn warnings(&self) -> Vec<Warning> {
        let rooms = match self {
            Self::Filesystem { room, capacity, .. } => vec![(Source::Disk, Some(*room)), (Source::Capacity, *capacity)],
            Self::Capacity { room, .. } => vec![(Source::Capacity, Some(*room))],
            Self::NoCapacity { .. } | Self::NoNumbers { .. } | Self::Unknown => Vec::new(),
        };
        rooms.into_iter().filter_map(|(source, room)| room.filter(|room| room.near()).map(|room| Warning { source, room })).collect()
    }
}

/// What the "all storage" line is a measure of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// The store's filesystem: real bytes on the disk.
    Disk,
    /// What the accounts have stored, of the capacity set. It counts per account (a chunk two accounts share counts twice,
    /// and so do uploads that were refused), so it is an upper bound of what the store holds.
    Accounts,
    /// Nothing to measure it against.
    Unmeasured,
}

/// The server storage block of the admin pages, worked out from [`AdminStorageOverview`]. Every number the server left out
/// (an older server, Postgres, S3's filesystem) is `None` here and shown as missing, never as zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageBlock {
    /// What the accounts have stored, counted per account (the overview's `stored_bytes`).
    pub stored: u64,
    /// The room of the "all storage" line, and what it measures.
    pub all: Option<Room>,
    pub basis: Basis,
    /// What is left of it.
    pub free: Option<u64>,
    /// The capacity the admin set, if any.
    pub capacity: Option<u64>,
    /// The filesystem of the server's own data, with the database's size.
    pub server_disk: Option<Room>,
    pub database_bytes: Option<u64>,
    pub store: StoreRoom,
}

/// A filesystem's used and total bytes from its total and free bytes; `None` if either is missing or the total is 0.
fn filesystem(total: Option<u64>, free: Option<u64>) -> Option<Room> {
    let (total, free) = (total?, free?);
    (total > 0).then(|| Room { used: total.saturating_sub(free), total })
}

impl StorageBlock {
    pub fn of(overview: &AdminStorageOverview) -> Self {
        let stored = overview.stored_bytes;
        // A capacity of 0 can't be (the server refuses it): it reads as not set rather than as a division by nothing.
        let capacity = overview.capacity_bytes.filter(|capacity| *capacity > 0);
        let against_capacity = capacity.map(|total| Room { used: stored, total });
        let store_fs = filesystem(overview.store_fs_total_bytes, overview.store_fs_free_bytes);
        let kind = overview.store_kind.clone().unwrap_or_default();
        let store = match (kind.as_str(), store_fs, against_capacity) {
            // Only a disk store has a filesystem of its own to read.
            ("disk" | "", Some(room), capacity) => StoreRoom::Filesystem { kind: "disk".into(), room, capacity },
            ("disk", None, None) => StoreRoom::NoNumbers { kind: kind.clone() },
            ("", None, None) => StoreRoom::Unknown,
            (_, _, Some(room)) => StoreRoom::Capacity { kind: kind.clone(), room },
            (_, _, None) => StoreRoom::NoCapacity { kind: kind.clone() },
        };
        let (all, basis) = match &store {
            StoreRoom::Filesystem { room, .. } => (Some(*room), Basis::Disk),
            StoreRoom::Capacity { room, .. } => (Some(*room), Basis::Accounts),
            _ => (None, Basis::Unmeasured),
        };
        Self {
            stored,
            all,
            basis,
            free: all.map(|room| room.total.saturating_sub(room.used)),
            capacity,
            server_disk: filesystem(overview.server_fs_total_bytes, overview.server_fs_free_bytes),
            database_bytes: overview.database_bytes,
            store,
        }
    }

    /// How many of the rows (the server disk, the store) have a room at or past the warning share.
    pub fn near_count(&self) -> usize {
        usize::from(self.server_disk.is_some_and(Room::near)) + usize::from(!self.store.warnings().is_empty())
    }
}

impl From<AdminEvent> for Event {
    fn from(event: AdminEvent) -> Self {
        Event::Admin(event)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::agent::fake::{FakeAgent, sample_user};
    use crate::agent::{Starter, call};
    use crate::audit::AuditMsg;
    use crate::model::{JoinDefaults, Tab, tests::status};

    /// An agent signed in as `fabio`, an admin (or not), with `others` more accounts after fabio's own.
    pub(crate) fn agent(admin: bool, others: usize) -> FakeAgent {
        let agent = FakeAgent::new(status(true));
        agent.admin.store(admin, std::sync::atomic::Ordering::SeqCst);
        let mut users = vec![AdminUserInfo { role: "admin".into(), ..sample_user("fabio", Uuid::now_v7()) }];
        users.extend((1..=others).map(|n| sample_user(&format!("user{n:02}"), Uuid::now_v7())));
        *agent.users.lock().unwrap() = users;
        agent
    }

    /// Feeds `events` to the model, each effect answered by the fake until nothing is left (as the link does).
    pub(crate) fn run(model: &mut Model, agent: &FakeAgent, events: Vec<Event>) {
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

    pub(crate) fn signed_in(agent: &FakeAgent) -> Model {
        let mut model = Model::new(JoinDefaults::default(), true, "run.log".into());
        run(&mut model, agent, vec![Event::Start]);
        model
    }

    /// The model on the admin pages.
    pub(crate) fn on_the_pages(agent: &FakeAgent) -> Model {
        let mut model = signed_in(agent);
        run(&mut model, agent, vec![Event::Admin(AdminEvent::Open)]);
        assert!(model.admin_screen().is_some(), "{:?}", model.screen);
        model
    }

    pub(crate) fn admin(model: &Model) -> &AdminScreen {
        model.admin_screen().unwrap_or_else(|| panic!("not on the admin pages: {:?}", model.screen))
    }

    pub(crate) fn admin_mut(model: &mut Model) -> &mut AdminScreen {
        match &mut model.screen {
            Screen::Admin(admin) => admin,
            other => panic!("not on the admin pages: {other:?}"),
        }
    }

    pub(crate) fn user_named(model: &Model, name: &str) -> AdminUserInfo {
        admin(model).list.users.iter().find(|u| u.username == name).cloned().unwrap_or_else(|| panic!("no {name} in the list"))
    }

    pub(crate) fn ev(event: AdminEvent) -> Vec<Event> {
        vec![Event::Admin(event)]
    }

    pub(crate) fn type_password(model: &mut Model, password: &str) {
        admin_mut(model).dialog.as_mut().unwrap().confirmation.password = password.to_owned();
    }

    pub(crate) fn refused(code: &str, message: &str) -> Error {
        Error::Refused { code: code.into(), message: message.into(), name_taken: None }
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
        assert_eq!(member.asked.lock().unwrap().iter().filter(|a| *a == "account").count(), 1);

        let boss = agent(true, 3);
        let mut model = signed_in(&boss);
        assert!(model.is_admin());
        assert_eq!(model.overview().unwrap().access, Access::Admin { two_factor: false });
        let effects = model.update(Event::Admin(AdminEvent::Open));
        assert!(matches!(effects.as_slice(), [Effect::AdminUsers { .. }, Effect::AdminServer { .. }, Effect::AdminStorage { .. }]), "{effects:?}");
        assert!(model.admin_screen().is_some());
        // The counters come with the pages, not with the sign-in.
        assert!(model.admin_screen().unwrap().server.is_none());
        let mut model = on_the_pages(&boss);
        let info = model.admin_screen().unwrap().server.clone().expect("the counters");
        assert_eq!((info.users, info.storage_kind.as_str()), (4, "disk"));
        let _ = &mut model;

        // A server that can't be reached says nothing about the role: no pages, asked again at the next refresh.
        let mut model = Model::new(JoinDefaults::default(), true, String::new());
        model.update(Event::Start);
        model.update(Event::Found(Ok(status(true))));
        let session = model.overview().unwrap().session;
        model.update(Event::Admin(AdminEvent::Account { session, result: Err(Error::Io("reset".into())) }));
        assert_eq!(model.overview().unwrap().access, Access::Unknown);
        assert!(model.update(Event::RefreshDevices).contains(&Effect::Account { session }));
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
        assert_eq!(dialog.error.as_deref(), Some("That password isn't right."));
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
        assert!(text.contains("Too many tries") && text.contains("90 seconds"), "{text}");
        let busy = Error::RefusedWith {
            code: codes::RATE_LIMITED.into(),
            message: "the server is busy".into(),
            two_factor: None,
            retry_after_secs: Some(600),
        };
        assert!(error_text(&busy).contains("10 minutes"));
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
        assert_eq!(model.update(answer), vec![Effect::Account { session: model.overview().unwrap().session }]);
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
            (Action::IssueReset { id: ana.id, username: "user01".into(), reset_two_factor: false }, CodeKind::PasswordReset, "TUVWXYZ234567ABC"),
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
        let confirmation = Confirmation { password: "admin-secret-pw".into(), code: "918273".into() };
        let shown = format!("{confirmation:?}");
        assert!(!shown.contains("admin-secret-pw") && !shown.contains("918273"), "{shown}");
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

    fn account(role: &str) -> AccountInfo {
        AccountInfo {
            user_id: Uuid::nil(),
            username: "fabio".into(),
            role: role.into(),
            created_ms: 1,
            password_changed_ms: None,
            two_factor_enabled: false,
        }
    }

    /// An answer that comes late (the pages were left and opened again; the devices were asked for twice) matches
    /// nothing: it never overwrites newer data.
    #[test]
    fn answers_that_come_late_are_ignored() {
        let boss = agent(true, 3);
        let mut model = signed_in(&boss);
        let first = model.update(Event::Admin(AdminEvent::Open));
        let Some(Effect::AdminUsers { ticket: old, .. }) = first.first().cloned() else { panic!("{first:?}") };
        model.update(Event::Admin(AdminEvent::Close));
        run(&mut model, &boss, ev(AdminEvent::Open));
        let current = admin(&model).list.users.clone();
        assert_eq!(current.len(), 4);
        let ghost = AdminUserList { users: vec![sample_user("ghost", Uuid::now_v7())], next: None };
        model.update(Event::Admin(AdminEvent::Users { ticket: old, result: Ok(ghost) }));
        assert_eq!(admin(&model).list.users, current, "the first page's reply is from before the pages were opened again");

        // The same for a user's devices.
        let ana = user_named(&model, "user01");
        let asked = model.update(Event::Admin(AdminEvent::OpenUser(ana.id)));
        let [Effect::AdminDevices { ticket: first_devices, .. }, Effect::AdminUserUsage { ticket: first_usage, .. }] = asked.as_slice() else {
            panic!("{asked:?}")
        };
        run(&mut model, &boss, ev(AdminEvent::RefreshDevices));
        run(&mut model, &boss, ev(AdminEvent::RefreshUsage));
        let View::User(view) = &admin(&model).view else { panic!() };
        let now = view.devices.clone();
        let usage_now = view.usage.clone();
        assert_eq!(now.as_ref().map(Vec::len), Some(1));
        model.update(Event::Admin(AdminEvent::Devices { user: ana.id, ticket: *first_devices, result: Ok(Vec::new()) }));
        let View::User(view) = &admin(&model).view else { panic!() };
        assert_eq!(view.devices, now);
        model.update(Event::Admin(AdminEvent::Usage {
            user: ana.id,
            ticket: *first_usage,
            result: Ok(craft_cloud_app::AccountUsage {
                stored_bytes: 1,
                quota_bytes: None,
                trash_bytes: Some(2),
                history_bytes: Some(3),
                estimates: craft_cloud_app::UsageEstimates { index_complete: true, reason: None },
            }),
        }));
        let View::User(view) = &admin(&model).view else { panic!() };
        assert_eq!(view.usage, usage_now);
    }

    /// The admin check of an earlier sign-in (another account, maybe) says nothing about the current one.
    #[test]
    fn an_admin_check_for_an_earlier_sign_in_is_ignored() {
        let mut model = Model::new(JoinDefaults::default(), true, String::new());
        model.update(Event::Start);
        let probe = |effects: &[Effect]| {
            effects.iter().find_map(|e| if let Effect::Account { session } = e { Some(*session) } else { None }).expect("an admin check")
        };
        let first = probe(&model.update(Event::Found(Ok(status(true)))));
        model.update(Event::SignedOut(Ok(status(false))));
        let second = probe(&model.update(Event::Polled(Ok(status(true)))));
        assert_ne!(first, second);
        // The first account was an admin; its late answer doesn't make this one an admin.
        model.update(Event::Admin(AdminEvent::Account { session: first, result: Ok(account("admin")) }));
        assert_eq!(model.overview().unwrap().access, Access::Unknown);
        assert!(!model.is_admin());
        model.update(Event::Admin(AdminEvent::Account { session: second, result: Ok(account("member")) }));
        assert_eq!(model.overview().unwrap().access, Access::Member);
        // And a late "member" answer doesn't take an admin's pages away.
        model.update(Event::Admin(AdminEvent::Account { session: first, result: Ok(account("member")) }));
        model.update(Event::Admin(AdminEvent::Account { session: second, result: Ok(account("admin")) }));
        assert!(model.is_admin());
        model.update(Event::Admin(AdminEvent::Account { session: first, result: Ok(account("member")) }));
        assert!(model.is_admin());
    }

    /// A password reset code can be revoked like an invite: the button doesn't depend on `invite_pending` (which
    /// doesn't say there is one), and a revoke that finds nothing open says so and looks at the list again.
    #[test]
    fn a_revoke_that_finds_no_open_code_says_so() {
        let boss = agent(true, 1);
        let mut model = on_the_pages(&boss);
        let ana = user_named(&model, "user01");
        assert!(!ana.invite_pending);
        run(&mut model, &boss, ev(AdminEvent::OpenUser(ana.id)));
        run(&mut model, &boss, ev(AdminEvent::Ask(Action::IssueReset { id: ana.id, username: "user01".into(), reset_two_factor: false })));
        type_password(&mut model, "pw");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        run(&mut model, &boss, ev(AdminEvent::DismissCode));
        // The reset code is open, though the list doesn't say so: it can be revoked.
        run(&mut model, &boss, ev(AdminEvent::Ask(Action::RevokeInvite { id: ana.id, username: "user01".into() })));
        type_password(&mut model, "pw");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        assert!(admin(&model).dialog.is_none());
        assert_eq!(boss.asked.lock().unwrap().iter().filter(|a| *a == "admin_revoke_invite").count(), 1);
        // Nothing open (the server's `not_found`): said clearly, and the list is asked again.
        boss.admin_fails.lock().unwrap().push(refused(codes::NOT_FOUND, "not found"));
        run(&mut model, &boss, ev(AdminEvent::Ask(Action::RevokeInvite { id: ana.id, username: "user01".into() })));
        type_password(&mut model, "pw");
        let effects = model.update(Event::Admin(AdminEvent::Confirm));
        let start: Starter = Arc::new(|_: &dyn crate::agent::Agent| Err(String::new()));
        let answer = call(&boss, &start, effects.into_iter().next().unwrap()).unwrap();
        let effects = model.update(answer);
        assert_eq!(admin(&model).dialog.as_ref().unwrap().error.as_deref(), Some(NO_OPEN_CODE));
        assert!(matches!(effects.as_slice(), [Effect::AdminUsers { .. }]), "{effects:?}");
    }

    /// An admin whose account has two-factor sign-in confirms with a code too: one field, six digits are a code from the app and
    /// anything else a recovery code; a missing code is not sent; everything typed leaves the model when it is sent.
    #[test]
    fn a_confirmation_asks_for_a_code_when_the_account_has_two_factor() {
        let boss = agent(true, 1);
        boss.two_factor.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut model = on_the_pages(&boss);
        assert_eq!(model.overview().unwrap().access, Access::Admin { two_factor: true });
        let ana = user_named(&model, "user01");
        let type_in = |model: &mut Model, password: &str, code: &str| {
            let d = admin_mut(model).dialog.as_mut().unwrap();
            d.confirmation.password = password.into();
            d.confirmation.code = code.into();
        };
        run(&mut model, &boss, ev(AdminEvent::Ask(Action::Disable { id: ana.id, username: "user01".into() })));
        assert!(admin(&model).dialog.as_ref().unwrap().needs_code);
        // A password alone, or a code alone, is not sent.
        type_in(&mut model, "admin-secret-pw", "");
        assert!(model.update(Event::Admin(AdminEvent::Confirm)).is_empty());
        assert_eq!(admin(&model).dialog.as_ref().unwrap().error.as_deref(), Some(NEEDS_CODE));
        type_in(&mut model, "", "123456");
        assert!(model.update(Event::Admin(AdminEvent::Confirm)).is_empty());
        assert_eq!(admin(&model).dialog.as_ref().unwrap().error.as_deref(), Some(NEEDS_PASSWORD));
        // Six digits with spaces: a code from the app.
        type_in(&mut model, "admin-secret-pw", " 123 456 ");
        let effects = model.update(Event::Admin(AdminEvent::Confirm));
        let [Effect::AdminDo { confirm, .. }] = effects.as_slice() else { panic!("{effects:?}") };
        assert_eq!(
            (confirm.password.as_deref(), confirm.totp_code.as_deref(), confirm.recovery_code.as_deref()),
            (Some("admin-secret-pw"), Some("123456"), None)
        );
        let dialog = admin(&model).dialog.as_ref().unwrap();
        assert!(dialog.busy && dialog.confirmation.password.is_empty() && dialog.confirmation.code.is_empty(), "{dialog:?}");
        let shown = format!("{model:?} {effects:?}");
        assert!(!shown.contains("admin-secret-pw") && !shown.contains("123456"), "{shown}");
        run(&mut model, &boss, vec![]);
        let start: Starter = Arc::new(|_: &dyn crate::agent::Agent| Err(String::new()));
        let answer = call(&boss, &start, effects.into_iter().next().unwrap()).unwrap();
        model.update(answer);
        assert_eq!(boss.factors.lock().unwrap().as_slice(), [(Some("123456".to_owned()), None)]);
        // Anything else is a recovery code, in the other field.
        run(&mut model, &boss, ev(AdminEvent::Ask(Action::Enable { id: ana.id, username: "user01".into() })));
        type_in(&mut model, "admin-secret-pw", "abcd-efgh-jkmn-pqrs");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        assert_eq!(boss.factors.lock().unwrap().last().unwrap(), &(None, Some("abcd-efgh-jkmn-pqrs".to_owned())));
        // The password dialog of an account without it has no code field.
        let plain = agent(true, 1);
        let mut model = on_the_pages(&plain);
        run(&mut model, &plain, ev(AdminEvent::Ask(Action::Disable { id: ana.id, username: "user01".into() })));
        assert!(!admin(&model).dialog.as_ref().unwrap().needs_code);
    }

    /// The server's word on a code: a wrong one (403) is said and the page asks for the code if it didn't; a lock (429) says how
    /// long to wait; the typed secrets are gone either way.
    #[test]
    fn a_wrong_or_locked_code_is_said_and_asked_for_again() {
        let boss = agent(true, 1);
        let mut model = on_the_pages(&boss);
        let ana = user_named(&model, "user01");
        assert!(!{
            run(&mut model, &boss, ev(AdminEvent::Ask(Action::Disable { id: ana.id, username: "user01".into() })));
            admin(&model).dialog.as_ref().unwrap().needs_code
        });
        boss.admin_fails.lock().unwrap().push(refused(codes::TWO_FACTOR_INVALID, "wrong code"));
        type_password(&mut model, "pw");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        let dialog = admin(&model).dialog.as_ref().unwrap();
        assert!(dialog.needs_code, "the server wants a code: the page asks for it");
        assert!(dialog.error.as_deref().is_some_and(|e| e.contains("code isn't right")), "{:?}", dialog.error);
        assert!(!dialog.busy && dialog.confirmation.password.is_empty() && dialog.confirmation.code.is_empty());
        let locked =
            Error::RefusedWith { code: codes::TWO_FACTOR_LOCKED.into(), message: "locked".into(), two_factor: None, retry_after_secs: Some(840) };
        boss.admin_fails.lock().unwrap().push(locked.clone());
        type_password(&mut model, "pw");
        admin_mut(&mut model).dialog.as_mut().unwrap().confirmation.code = "123456".into();
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        let error = admin(&model).dialog.as_ref().unwrap().error.clone().unwrap();
        assert!(error.contains("Too many wrong codes") && error.contains("14 minutes"), "{error}");
        assert!(error_text(&locked).contains("14 minutes"));
    }

    /// An account's two-factor sign-in is shown, and taken away with a change of its own; a reset code can turn it off when it is
    /// used, which the call carries.
    #[test]
    fn two_factor_is_shown_reset_and_turned_off_by_a_reset_code() {
        let boss = agent(true, 1);
        boss.users.lock().unwrap()[1].two_factor_enabled = true;
        let mut model = on_the_pages(&boss);
        let ana = user_named(&model, "user01");
        assert!(ana.two_factor_enabled);
        run(&mut model, &boss, ev(AdminEvent::OpenUser(ana.id)));
        let reset = Action::ResetTwoFactor { id: ana.id, username: "user01".into() };
        assert!(reset.title().contains("Turn two-factor sign-in off") && reset.explain().contains("signed out"));
        run(&mut model, &boss, ev(AdminEvent::Ask(reset)));
        type_password(&mut model, "pw");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        assert!(admin(&model).dialog.is_none());
        assert_eq!(boss.asked.lock().unwrap().iter().filter(|a| *a == "admin_reset_two_factor").count(), 1);
        for flag in [false, true] {
            run(&mut model, &boss, ev(AdminEvent::Ask(Action::IssueReset { id: ana.id, username: "user01".into(), reset_two_factor: flag })));
            type_password(&mut model, "pw");
            run(&mut model, &boss, ev(AdminEvent::Confirm));
            assert_eq!(admin(&model).code.as_ref().map(|c| c.kind), Some(CodeKind::PasswordReset));
            run(&mut model, &boss, ev(AdminEvent::DismissCode));
        }
        assert_eq!(boss.resets.lock().unwrap().as_slice(), [false, true], "the flag goes with the reset code");
        assert!(Action::IssueReset { id: ana.id, username: "x".into(), reset_two_factor: true }.explain().contains("turns the account's two-factor"));
    }

    /// The model on the account page of `user01`, and a way to type into its fields.
    fn on_an_account(agent: &FakeAgent) -> (Model, Uuid) {
        let mut model = on_the_pages(agent);
        let id = user_named(&model, "user01").id;
        run(&mut model, agent, ev(AdminEvent::OpenUser(id)));
        (model, id)
    }

    fn type_into_the_account(model: &mut Model, role: RoleChoice, quota: &str) {
        if let View::User(view) = &mut admin_mut(model).view {
            view.role = role;
            view.quota = quota.into();
        }
    }

    /// Role and quota edits that are typed and not saved are told from a page that shows what is saved.
    #[test]
    fn typed_role_and_quota_edits_make_the_account_page_dirty_until_they_are_saved_or_undone() {
        let boss = agent(true, 2);
        let (mut model, _) = on_an_account(&boss);
        assert!(!admin(&model).has_unsaved_edits());
        type_into_the_account(&mut model, RoleChoice::Member, "  ");
        assert!(!admin(&model).has_unsaved_edits(), "blanks are no edit");
        type_into_the_account(&mut model, RoleChoice::Admin, "");
        assert!(admin(&model).has_unsaved_edits(), "a role");
        type_into_the_account(&mut model, RoleChoice::Member, "7");
        assert!(admin(&model).has_unsaved_edits(), "a quota");
        type_into_the_account(&mut model, RoleChoice::Member, "");
        assert!(!admin(&model).has_unsaved_edits(), "typed back to what was saved");
        // A save makes the page what the server answered: clean.
        type_into_the_account(&mut model, RoleChoice::Member, "3");
        model.update(Event::Admin(AdminEvent::SubmitEdit));
        type_password(&mut model, "pw");
        run(&mut model, &boss, ev(AdminEvent::Confirm));
        let View::User(view) = &admin(&model).view else { panic!("not on the account") };
        assert_eq!((view.quota.as_str(), view.quota_shown.as_str()), ("3", "3"));
        assert!(!admin(&model).has_unsaved_edits());
        // The edit being sent is no unsaved edit, but one waiting for the password is.
        type_into_the_account(&mut model, RoleChoice::Member, "4");
        model.update(Event::Admin(AdminEvent::SubmitEdit));
        assert!(admin(&model).has_unsaved_edits());
        type_password(&mut model, "pw");
        let effects = model.update(Event::Admin(AdminEvent::Confirm));
        assert!(matches!(effects.as_slice(), [Effect::AdminDo { .. }]));
        assert!(!admin(&model).has_unsaved_edits(), "being sent");
    }

    /// A tab, another account, the list, the audit log, Sign out and Quit all wait for the answer while edits are unsaved.
    #[test]
    fn leaving_an_account_page_with_unsaved_edits_asks_first() {
        let boss = agent(true, 2);
        let (mut model, id) = on_an_account(&boss);
        let other = user_named(&model, "user02").id;
        let ana = user_named(&model, "user01").username;
        // Clean: everything goes through.
        model.update(Event::Tab(Tab::Overview));
        assert_eq!(model.tab(), Some(Tab::Overview));
        let (mut model, _) = on_an_account(&boss);
        type_into_the_account(&mut model, RoleChoice::Admin, "");
        model.window_visible = false;
        let leaving = [
            Event::Tab(Tab::Trash),
            Event::Tab(Tab::Overview),
            Event::SignOut,
            Event::SignOutHere,
            Event::Quit,
            Event::CloseRequested,
            Event::Exit,
            Event::Admin(AdminEvent::BackToList),
            Event::Admin(AdminEvent::Close),
            Event::Admin(AdminEvent::OpenUser(other)),
            Event::Audit(AuditMsg::OpenAudit(Some((id, ana)))),
        ];
        for event in leaving {
            assert!(model.update(event.clone()).is_empty(), "{event:?} waits");
            assert!(matches!(model.screen, Screen::Admin(_)), "{event:?} stays on the page");
            assert_eq!(admin(&model).leaving.as_deref(), Some(&event));
            assert!(model.window_visible, "the question is seen");
            model.window_visible = false;
        }
        // Things that keep the page, and the page's own tab, are not questions.
        admin_mut(&mut model).leaving = None;
        for event in [Event::Tab(Tab::Admin), Event::Admin(AdminEvent::OpenUser(id)), Event::Admin(AdminEvent::RefreshUsage), Event::Pause] {
            model.update(event);
            assert!(admin(&model).leaving.is_none());
        }
    }

    /// Ending the app is a way out of the page too: the close button without a tray (the window would end with the edits)
    /// and the Linux tray process's Exit ask first, and Discard then ends the app.
    #[test]
    fn ending_the_app_with_unsaved_edits_asks_first_and_discard_ends_it() {
        let boss = agent(true, 2);
        for (tray, event, then) in [
            (false, Event::CloseRequested, vec![Effect::QuitApp]),
            (false, Event::Exit, vec![Effect::QuitApp]),
            (true, Event::CloseRequested, Vec::new()),
        ] {
            let (mut model, _) = on_an_account(&boss);
            model.tray = tray;
            type_into_the_account(&mut model, RoleChoice::Admin, "9");
            model.window_visible = false;
            assert!(model.update(event.clone()).is_empty(), "{event:?} waits: no effects");
            assert!(matches!(model.screen, Screen::Admin(_)));
            assert_eq!(admin(&model).leaving.as_deref(), Some(&event), "kept pending");
            assert!(model.window_visible, "the window is shown for the question, tray or not");
            // Keep editing drops it and nothing ends.
            run(&mut model, &boss, ev(AdminEvent::KeepEditing));
            assert!(admin(&model).leaving.is_none() && admin(&model).has_unsaved_edits());
            assert!(model.window_visible);
            // Asked again, Discard carries it out.
            assert!(model.update(event.clone()).is_empty());
            assert_eq!(model.update(Event::Admin(AdminEvent::DiscardEdits)), then, "{event:?}");
        }
        // Without edits they go through at once.
        let (mut model, _) = on_an_account(&boss);
        model.tray = false;
        assert_eq!(model.update(Event::CloseRequested), vec![Effect::QuitApp]);
        assert_eq!(model.update(Event::Exit), vec![Effect::QuitApp]);
    }

    /// Keep editing drops what was asked for and leaves the fields as typed; Discard sets them back and carries it out.
    #[test]
    fn keep_editing_stays_and_discard_goes_on_to_what_was_asked() {
        let boss = agent(true, 2);
        let (mut model, id) = on_an_account(&boss);
        type_into_the_account(&mut model, RoleChoice::Admin, "9");
        assert!(model.update(Event::Tab(Tab::Trash)).is_empty());
        run(&mut model, &boss, ev(AdminEvent::KeepEditing));
        assert!(admin(&model).leaving.is_none() && admin(&model).has_unsaved_edits());
        let View::User(view) = &admin(&model).view else { panic!() };
        assert_eq!((view.role, view.quota.as_str()), (RoleChoice::Admin, "9"));
        // Discard with nothing asked for does nothing.
        assert!(model.update(Event::Admin(AdminEvent::DiscardEdits)).is_empty());
        assert!(admin(&model).has_unsaved_edits());

        model.update(Event::Tab(Tab::Trash));
        run(&mut model, &boss, ev(AdminEvent::DiscardEdits));
        assert_eq!(model.tab(), Some(Tab::Trash), "the tab was opened");

        // Back to the account: the edits are gone with the page, and the other asks (the list) go through too.
        let (mut model, id2) = on_an_account(&boss);
        assert_eq!(id, id2);
        type_into_the_account(&mut model, RoleChoice::Admin, "9");
        model.update(Event::Admin(AdminEvent::BackToList));
        run(&mut model, &boss, ev(AdminEvent::DiscardEdits));
        assert!(matches!(admin(&model).view, View::List));
        run(&mut model, &boss, ev(AdminEvent::OpenUser(id)));
        let View::User(view) = &admin(&model).view else { panic!() };
        assert!(!view.is_dirty());
    }

    /// A page that waits for a password or a call, or shows a code, is not asked about: those have their own rules.
    #[test]
    fn a_page_that_is_locked_does_not_ask() {
        let boss = agent(true, 2);
        let (mut model, _) = on_an_account(&boss);
        type_into_the_account(&mut model, RoleChoice::Admin, "");
        model.update(Event::Admin(AdminEvent::SubmitEdit));
        type_password(&mut model, "pw");
        model.update(Event::Admin(AdminEvent::Confirm));
        assert!(model.tabs_locked());
        assert!(model.update(Event::Quit).is_empty());
        assert!(admin(&model).leaving.is_none(), "a save under way answers by itself");
    }
}

#[cfg(test)]
mod storage_block_tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn overview(f: impl FnOnce(&mut AdminStorageOverview)) -> StorageBlock {
        let mut overview = AdminStorageOverview { stored_bytes: 100 * GIB, ..Default::default() };
        f(&mut overview);
        StorageBlock::of(&overview)
    }

    fn disk(o: &mut AdminStorageOverview, total: u64, free: u64) {
        o.store_kind = Some("disk".into());
        o.store_fs_total_bytes = Some(total * GIB);
        o.store_fs_free_bytes = Some(free * GIB);
    }

    fn texts(block: &StorageBlock) -> Vec<String> {
        block.store.warnings().into_iter().map(Warning::text).collect()
    }

    #[test]
    fn a_disk_store_is_measured_by_its_filesystem_and_the_capacity_is_a_second_measure() {
        let block = overview(|o| {
            disk(o, 1000, 600);
            o.capacity_bytes = Some(400 * GIB);
            o.server_fs_total_bytes = Some(80 * GIB);
            o.server_fs_free_bytes = Some(70 * GIB);
            o.database_bytes = Some(GIB);
        });
        assert_eq!((block.all, block.basis, block.free), (Some(Room { used: 400 * GIB, total: 1000 * GIB }), Basis::Disk, Some(600 * GIB)));
        assert_eq!(block.capacity, Some(400 * GIB));
        assert_eq!(
            block.store,
            StoreRoom::Filesystem {
                kind: "disk".into(),
                room: Room { used: 400 * GIB, total: 1000 * GIB },
                capacity: Some(Room { used: 100 * GIB, total: 400 * GIB })
            }
        );
        assert_eq!(block.server_disk, Some(Room { used: 10 * GIB, total: 80 * GIB }));
        assert_eq!(block.database_bytes, Some(GIB));
        assert_eq!((texts(&block), block.near_count()), (Vec::<String>::new(), 0));
    }

    #[test]
    fn a_disk_store_without_capacity_has_the_filesystem_only() {
        let block = overview(|o| disk(o, 1000, 600));
        assert_eq!((block.all, block.basis, block.capacity), (Some(Room { used: 400 * GIB, total: 1000 * GIB }), Basis::Disk, None));
        assert!(matches!(block.store, StoreRoom::Filesystem { capacity: None, .. }));
    }

    #[test]
    fn a_disk_near_its_capacity_warns_with_the_capacity_and_a_full_one_says_so() {
        let near = overview(|o| {
            disk(o, 1000, 600);
            o.capacity_bytes = Some(110 * GIB);
        });
        assert_eq!(texts(&near), ["capacity 90 %"]);
        assert_eq!(near.near_count(), 1);
        let over = overview(|o| {
            disk(o, 1000, 600);
            o.capacity_bytes = Some(80 * GIB);
        });
        assert_eq!(texts(&over), ["capacity 125 %"], "stored passes the capacity: a warning, never a bar past its end");
        assert!(over.store.warnings().iter().all(|w| w.room.full()));
    }

    #[test]
    fn a_nearly_full_filesystem_warns_with_the_disk() {
        let block = overview(|o| disk(o, 1000, 92));
        assert_eq!(texts(&block), ["disk 90 %"]);
        assert_eq!(block.near_count(), 1);
        let both = overview(|o| {
            disk(o, 100, 5);
            o.capacity_bytes = Some(100 * GIB);
        });
        assert_eq!(texts(&both), ["disk 95 %", "capacity 100 %"]);
        assert_eq!(both.near_count(), 1, "one store, two reasons");
    }

    #[test]
    fn stored_over_the_filesystem_is_not_a_warning_of_the_filesystem() {
        // Chunks that accounts share count once on the disk and once for each account.
        let block = overview(|o| disk(o, 50, 40));
        assert_eq!(block.all, Some(Room { used: 10 * GIB, total: 50 * GIB }));
        assert!(texts(&block).is_empty());
        assert_eq!(block.stored, 100 * GIB, "the per-account sum is still known");
    }

    #[test]
    fn an_s3_store_is_the_accounts_total_against_the_capacity() {
        let block = overview(|o| {
            o.store_kind = Some("s3".into());
            o.capacity_bytes = Some(200 * GIB);
        });
        assert_eq!(block.store, StoreRoom::Capacity { kind: "s3".into(), room: Room { used: 100 * GIB, total: 200 * GIB } });
        assert_eq!((block.basis, block.free), (Basis::Accounts, Some(100 * GIB)));
        let near = overview(|o| {
            o.store_kind = Some("s3".into());
            o.capacity_bytes = Some(110 * GIB);
        });
        assert_eq!(texts(&near), ["capacity 90 %"]);
        assert_eq!(near.near_count(), 1);
        let over = overview(|o| {
            o.store_kind = Some("s3".into());
            o.capacity_bytes = Some(50 * GIB);
        });
        assert_eq!((texts(&over), over.free), (vec!["capacity 200 %".to_owned()], Some(0)), "no negative free");
    }

    #[test]
    fn an_s3_or_memory_store_without_capacity_has_no_size_and_no_free() {
        for kind in ["s3", "memory", "other"] {
            let block = overview(|o| o.store_kind = Some(kind.into()));
            assert_eq!(block.store, StoreRoom::NoCapacity { kind: kind.into() });
            assert_eq!((block.all, block.free, block.basis, block.near_count()), (None, None, Basis::Unmeasured, 0));
            assert_eq!(block.store.kind(), Some(kind));
        }
    }

    #[test]
    fn an_old_server_leaves_everything_unknown() {
        let block = overview(|_| {});
        assert_eq!(block.store, StoreRoom::Unknown);
        assert_eq!((block.all, block.free, block.server_disk, block.database_bytes, block.store.kind()), (None, None, None, None, None));
        assert_eq!(block.near_count(), 0);
    }

    #[test]
    fn a_disk_store_without_numbers_stays_a_disk() {
        let block = overview(|o| o.store_kind = Some("disk".into()));
        assert_eq!(block.store, StoreRoom::NoNumbers { kind: "disk".into() });
        assert_eq!((block.store.kind(), block.all), (Some("disk"), None));
        // Half a filesystem is none, and a capacity of 0 is not one.
        let half = overview(|o| {
            o.store_kind = Some("disk".into());
            o.store_fs_total_bytes = Some(1000 * GIB);
            o.capacity_bytes = Some(0);
        });
        assert_eq!(half.store, StoreRoom::NoNumbers { kind: "disk".into() });
        assert_eq!(half.capacity, None);
        // A disk whose filesystem can't be read still has its capacity.
        let capacity = overview(|o| {
            o.store_kind = Some("disk".into());
            o.capacity_bytes = Some(200 * GIB);
        });
        assert_eq!((capacity.basis, capacity.all), (Basis::Accounts, Some(Room { used: 100 * GIB, total: 200 * GIB })));
    }

    #[test]
    fn postgres_has_no_database_size_or_server_disk() {
        let block = overview(|o| disk(o, 1000, 600));
        assert_eq!((block.server_disk, block.database_bytes), (None, None));
        assert!(block.store.room().is_some(), "the store's own numbers stay");
    }

    #[test]
    fn the_warning_starts_at_ninety_percent() {
        let room = |used| Room { used, total: 1000 };
        assert!(!room(899).near());
        assert!(room(900).near());
        assert!(room(1000).near() && room(1000).full());
        assert!(room(1500).near() && room(1500).full());
        assert_eq!((room(899).percent(), room(900).percent(), room(1500).percent()), (89, 90, 150));
        assert_eq!(room(1500).fraction(), 1.0, "a bar never passes its end");
        assert!(!Room { used: 5, total: 0 }.near(), "no size, no warning");
        assert_eq!(Room { used: 5, total: 0 }.percent(), 0);
        assert!(Room { used: u64::MAX, total: u64::MAX }.near(), "no overflow at the top");
        let server = overview(|o| {
            o.server_fs_total_bytes = Some(100 * GIB);
            o.server_fs_free_bytes = Some(10 * GIB);
        });
        assert_eq!(server.near_count(), 1, "the server's own disk counts");
    }
}
